use crate::internal::messaging::Msg;
use crate::types::{Endpoint, GossipSeed, GossipSeedClusterSettings, NodePreference};
use futures::channel::mpsc;
use futures::sink::SinkExt;
use futures::stream::StreamExt;
use rand;
use rand::seq::SliceRandom;
use std::iter::FromIterator;
use std::net::{AddrParseError, SocketAddr};
use std::time::Duration;
use uuid::Uuid;

pub(crate) async fn discover(
    mut consumer: mpsc::Receiver<Option<Endpoint>>,
    sender: mpsc::Sender<Msg>,
    settings: GossipSeedClusterSettings,
) {
    let preference = NodePreference::Random;
    let client = reqwest::Client::new();
    let mut previous_candidates = None;

    async fn discover(
        client: &reqwest::Client,
        settings: &GossipSeedClusterSettings,
        previous_candidates: &mut Option<Vec<Member>>,
        preference: NodePreference,
        failed_endpoint: Option<Endpoint>,
    ) -> Option<NodeEndpoints> {
        let candidates = match previous_candidates.take() {
            Some(old_candidates) => candidates_from_old_gossip(failed_endpoint, old_candidates),

            None => candidates_from_dns(&settings),
        };

        let mut outcome = None;

        for candidate in candidates {
            let result = get_gossip_from(client, candidate).await;
            let result: std::io::Result<Vec<Member>> = result.and_then(|member_info| {
                let members: Vec<std::io::Result<Member>> = member_info
                    .into_iter()
                    .map(Member::from_member_info)
                    .collect();

                Result::from_iter(members)
            });

            match result {
                Err(error) => {
                    info!("candidate [{}] resolution error: {}", candidate, error);

                    continue;
                }

                Ok(members) => {
                    if members.is_empty() {
                        continue;
                    } else {
                        outcome = determine_best_node(preference, members.as_slice());

                        if outcome.is_some() {
                            *previous_candidates = Some(members);
                            break;
                        }

                        warn!("determine_best_node found no candidate!");
                    }
                }
            }
        }

        outcome
    }

    while let Some(failed_endpoint) = consumer.next().await {
        let mut att = 1usize;

        loop {
            if att > settings.max_discover_attempts {
                let err_msg = format!(
                    "Failed to discover candidate in {} attempts",
                    settings.max_discover_attempts
                );

                let err = std::io::Error::new(std::io::ErrorKind::NotFound, err_msg);
                let _ = sender
                    .clone()
                    .send(Msg::ConnectionClosed(Uuid::nil(), err))
                    .await;

                break;
            }

            let result_opt = discover(
                &client,
                &settings,
                &mut previous_candidates,
                preference,
                failed_endpoint,
            )
            .await;

            if let Some(node) = result_opt {
                let _ = sender.clone().send(Msg::Establish(node.tcp_endpoint)).await;

                break;
            }

            tokio::time::delay_for(Duration::from_millis(500)).await;
            att += 1;
        }
    }
}

fn candidates_from_dns(settings: &GossipSeedClusterSettings) -> Vec<GossipSeed> {
    // TODO - Currently we only shuffling from the initial seed list.
    // Later on, we will also try to get candidates from the DNS server
    // itself.
    let mut rng = rand::thread_rng();
    let mut src = settings.seeds.clone();

    src.shuffle(&mut rng);
    src.into_vec()
}

fn candidates_from_old_gossip(
    failed_endpoint: Option<Endpoint>,
    old_candidates: Vec<Member>,
) -> Vec<GossipSeed> {
    let candidates = match failed_endpoint {
        Some(endpoint) => old_candidates
            .into_iter()
            .filter(|member| member.external_tcp != endpoint.addr)
            .collect(),

        None => old_candidates,
    };

    arrange_gossip_candidates(candidates)
}

fn arrange_gossip_candidates(candidates: Vec<Member>) -> Vec<GossipSeed> {
    let mut arranged_candidates = Candidates::new();

    for member in candidates {
        arranged_candidates.push(member);
    }

    arranged_candidates.shuffle();
    arranged_candidates.gossip_seeds()
}

#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize, Copy, Clone)]
#[serde(rename_all = "PascalCase")]
enum VNodeState {
    Initializing,
    Unknown,
    PreReplica,
    CatchingUp,
    Clone,
    Slave,
    PreMaster,
    Master,
    Manager,
    ShuttingDown,
    Shutdown,
}

impl std::fmt::Display for VNodeState {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        use self::VNodeState::*;

        match self {
            Initializing => write!(f, "Initializing"),
            Unknown => write!(f, "Unknown"),
            PreReplica => write!(f, "PreReplica"),
            CatchingUp => write!(f, "CatchingUp"),
            Clone => write!(f, "Clone"),
            Slave => write!(f, "Slave"),
            PreMaster => write!(f, "PreMaster"),
            Master => write!(f, "Master"),
            Manager => write!(f, "Manager"),
            ShuttingDown => write!(f, "ShuttingDown"),
            Shutdown => write!(f, "Shutdown"),
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct Gossip {
    members: Vec<MemberInfo>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct MemberInfo {
    instance_id: Uuid,
    state: VNodeState,
    is_alive: bool,
    internal_tcp_ip: String,
    internal_tcp_port: u16,
    internal_secure_tcp_port: u16,
    external_tcp_ip: String,
    external_tcp_port: u16,
    external_secure_tcp_port: u16,
    internal_http_ip: String,
    internal_http_port: u16,
    external_http_ip: String,
    external_http_port: u16,
    last_commit_position: i64,
    writer_checkpoint: i64,
    chaser_checkpoint: i64,
    epoch_position: i64,
    epoch_number: i64,
    epoch_id: Uuid,
    node_priority: i64,
}

#[derive(Debug, Clone)]
struct Member {
    external_tcp: SocketAddr,
    external_secure_tcp: Option<SocketAddr>,
    external_http: SocketAddr,
    internal_tcp: SocketAddr,
    internal_secure_tcp: Option<SocketAddr>,
    internal_http: SocketAddr,
    state: VNodeState,
}

fn addr_parse_error_to_io_error(error: AddrParseError) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{}", error))
}

impl Member {
    fn from_member_info(info: MemberInfo) -> std::io::Result<Member> {
        let external_tcp = parse_socket_addr(format!(
            "{}:{}",
            info.external_tcp_ip, info.external_tcp_port
        ))?;

        let external_secure_tcp = {
            if info.external_secure_tcp_port < 1 {
                Ok(None)
            } else {
                parse_socket_addr(format!(
                    "{}:{}",
                    info.external_tcp_ip, info.external_secure_tcp_port
                ))
                .map(Some)
            }
        }?;

        let external_http = parse_socket_addr(format!(
            "{}:{}",
            info.external_http_ip, info.external_http_port
        ))?;

        let internal_tcp = parse_socket_addr(format!(
            "{}:{}",
            info.internal_tcp_ip, info.internal_tcp_port
        ))?;

        let internal_secure_tcp = {
            if info.internal_secure_tcp_port < 1 {
                Ok(None)
            } else {
                parse_socket_addr(format!(
                    "{}:{}",
                    info.internal_tcp_ip, info.internal_secure_tcp_port
                ))
                .map(Some)
            }
        }?;

        let internal_http = parse_socket_addr(format!(
            "{}:{}",
            info.internal_http_ip, info.internal_http_port
        ))?;

        let member = Member {
            external_tcp,
            external_secure_tcp,
            external_http,
            internal_tcp,
            internal_secure_tcp,
            internal_http,
            state: info.state,
        };

        Ok(member)
    }
}

fn parse_socket_addr(str_repr: String) -> std::io::Result<SocketAddr> {
    str_repr.parse().map_err(addr_parse_error_to_io_error)
}

struct Candidates {
    nodes: Vec<Member>,
    managers: Vec<Member>,
}

impl Candidates {
    fn new() -> Candidates {
        Candidates {
            nodes: vec![],
            managers: vec![],
        }
    }

    fn push(&mut self, member: Member) {
        if let VNodeState::Manager = member.state {
            self.managers.push(member);
        } else {
            self.nodes.push(member);
        }
    }

    fn shuffle(&mut self) {
        let mut rng = rand::thread_rng();

        self.nodes.shuffle(&mut rng);
        self.managers.shuffle(&mut rng);
    }

    fn gossip_seeds(mut self) -> Vec<GossipSeed> {
        self.nodes.extend(self.managers);

        self.nodes
            .into_iter()
            .map(|member| GossipSeed::from_socket_addr(member.external_http))
            .collect()
    }
}

pub(crate) struct NodeEndpoints {
    pub tcp_endpoint: Endpoint,
    pub secure_tcp_endpoint: Option<Endpoint>,
}

async fn get_gossip_from(
    client: &reqwest::Client,
    gossip: GossipSeed,
) -> std::io::Result<Vec<MemberInfo>> {
    let url = gossip.url()?;

    let result = client.get(url).send().await;

    let resp = result.map_err(|error| {
        let msg = format!("[{}] responded with [{}]", gossip, error);
        std::io::Error::new(std::io::ErrorKind::Other, msg)
    })?;

    match resp.json::<Gossip>().await {
        Ok(gossip) => Ok(gossip.members),
        Err(error) => {
            let msg = format!("[{}] responded with [{}]", gossip, error);
            Err(std::io::Error::new(std::io::ErrorKind::Other, msg))
        }
    }
}

fn determine_best_node(preference: NodePreference, members: &[Member]) -> Option<NodeEndpoints> {
    fn allowed_states(state: VNodeState) -> bool {
        match state {
            VNodeState::Manager | VNodeState::ShuttingDown | VNodeState::Shutdown => false,
            _ => true,
        }
    }

    let mut members: Vec<&Member> = members
        .iter()
        .filter(|member| allowed_states(member.state))
        .collect();

    members.as_mut_slice().sort_by(|a, b| a.state.cmp(&b.state));

    {
        let mut rng = rand::thread_rng();

        if let NodePreference::Random = preference {
            members.shuffle(&mut rng);
        }

        // TODO - Implement other node preferences.
    };

    let member_opt = members.into_iter().next();

    member_opt.map(|member| {
        info!(
            "Discovering: found best choice [{},{:?}] ({})",
            member.external_tcp, member.external_secure_tcp, member.state
        );

        NodeEndpoints {
            tcp_endpoint: Endpoint::from_addr(member.external_tcp),
            secure_tcp_endpoint: member.external_secure_tcp.map(Endpoint::from_addr),
        }
    })
}
