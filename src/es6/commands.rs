//! Commands this client supports.
use std::ops::Deref;
use std::collections::HashMap;

use futures::Stream;
use futures::stream::{self, TryStreamExt};

use crate::types::{self, OperationError, Slice};
use crate::es6::types::{ExpectedVersion, Position, EventData, WriteResult, Revision, ResolvedEvent, RecordedEvent, PersistentSubscriptionSettings};

use streams::append_req::options::ExpectedStreamRevision;
use streams::streams_client::StreamsClient;
use persistent::persistent_subscriptions_client::PersistentSubscriptionsClient;

pub mod streams {
    tonic::include_proto!("event_store.client.streams");
}

pub mod persistent {
    tonic::include_proto!("event_store.client.persistent_subscriptions");
}

use tonic::Request;
use tonic::transport::Channel;

fn convert_expected_version(
    version: ExpectedVersion,
) -> ExpectedStreamRevision {
    use streams::append_req::Empty;

    match version {
        ExpectedVersion::Any => ExpectedStreamRevision::Any(Empty{}),
        ExpectedVersion::StreamExists => ExpectedStreamRevision::StreamExists(Empty{}),
        ExpectedVersion::NoStream => ExpectedStreamRevision::NoStream(Empty{}),
        ExpectedVersion::Exact(version) => ExpectedStreamRevision::Revision(version),
    }
}

fn raw_uuid_to_uuid(
    src: streams::Uuid,
) -> uuid::Uuid {
    use byteorder::{ByteOrder, BigEndian};

    let value = src.value.expect("We expect Uuid value to be defined for now");

    match value {
        streams::uuid::Value::Structured(s) => {
            let mut buf = vec![];

            BigEndian::write_i64(&mut buf, s.most_significant_bits);
            BigEndian::write_i64(&mut buf, s.least_significant_bits);

            uuid::Uuid::from_slice(buf.as_slice())
                .expect("We expect a valid UUID out of byte buffer")
        }

        streams::uuid::Value::String(s) => {
            s.parse().expect("We expect a valid UUID out of this String")
        }
    }
}

fn convert_event_data(
    mut event: EventData,
) -> streams::AppendReq {
    use streams::append_req;

    let id = event.id_opt.unwrap_or_else(|| uuid::Uuid::new_v4());
    let id = streams::uuid::Value::String(id.to_string());
    let id = streams::Uuid {
        value: Some(id),
    };
    let is_json = event.payload.is_json();
    let mut metadata: HashMap<String, String> = HashMap::new();
    let custom_metadata = event
        .custom_metadata
        .map_or_else(|| vec![], |p| (&*p.into_inner()).into());

    metadata.insert("type".into(), event.event_type);
    metadata.insert("is-json".into(), format!("{}", is_json));

    let msg = append_req::ProposedMessage{
        id: Some(id),
        metadata,
        custom_metadata,
        data: (&*event.payload.into_inner()).into(),
    };

    let content = append_req::Content::ProposedMessage(msg);

    streams::AppendReq {
        content: Some(content),
    }
}

fn convert_proto_recorded_event(
    mut event: streams::read_resp::read_event::RecordedEvent,
) -> RecordedEvent {
    let id = event
        .id
        .map(raw_uuid_to_uuid)
        .expect("Unable to parse Uuid [convert_proto_recorded_event]");

    let position = Position {
        commit: event.commit_position,
        prepare: event.prepare_position,
    };

    let event_type =
        if let Some(tpe) = event.metadata.remove(&"type".to_owned()) {
            tpe
        } else {
            "<no-event-type-provided>".to_owned()
        };

    let is_json =
        if let Some(is_json) = event.metadata.remove(&"is-json".to_owned()) {
            match is_json.as_str() {
                "true" => true,
                "false" => false,
                unknown => panic!("Unknown [{}] 'is_json' metadata value"),
            }
        } else {
            false
        };

    RecordedEvent {
        id,
        stream_id: event.stream_name,
        revision: event.stream_revision,
        position,
        event_type,
        is_json,
        metadata: event.custom_metadata.into(),
        data: event.data.into(),
    }
}

fn convert_settings_create(
    settings: PersistentSubscriptionSettings,
) -> persistent::create_req::Settings {
    let named_consumer_strategy = match settings.named_consumer_strategy {
        types::SystemConsumerStrategy::DispatchToSingle => 0,
        types::SystemConsumerStrategy::RoundRobin => 1,
        types::SystemConsumerStrategy::Pinned => 2,
    };

    persistent::create_req::Settings {
        resolve_links: settings.resolve_links,
        revision: settings.revision,
        extra_statistics: settings.extra_stats,
        message_timeout: settings.message_timeout.as_millis() as i64,
        max_retry_count: settings.max_retry_count,
        checkpoint_after: settings.checkpoint_after.as_millis() as i64,
        min_checkpoint_count: settings.min_checkpoint_count,
        max_checkpoint_count: settings.max_checkpoint_count,
        max_subscriber_count: settings.max_subscriber_count,
        live_buffer_size: settings.live_buffer_size,
        read_batch_size: settings.read_batch_size,
        history_buffer_size: settings.history_buffer_size,
        named_consumer_strategy,
    }
}

fn convert_settings_update(
    settings: PersistentSubscriptionSettings,
) -> persistent::update_req::Settings {
    let named_consumer_strategy = match settings.named_consumer_strategy {
        types::SystemConsumerStrategy::DispatchToSingle => 0,
        types::SystemConsumerStrategy::RoundRobin => 1,
        types::SystemConsumerStrategy::Pinned => 2,
    };

    persistent::update_req::Settings {
        resolve_links: settings.resolve_links,
        revision: settings.revision,
        extra_statistics: settings.extra_stats,
        message_timeout: settings.message_timeout.as_millis() as i64,
        max_retry_count: settings.max_retry_count,
        checkpoint_after: settings.checkpoint_after.as_millis() as i64,
        min_checkpoint_count: settings.min_checkpoint_count,
        max_checkpoint_count: settings.max_checkpoint_count,
        max_subscriber_count: settings.max_subscriber_count,
        live_buffer_size: settings.live_buffer_size,
        read_batch_size: settings.read_batch_size,
        history_buffer_size: settings.history_buffer_size,
        named_consumer_strategy,
    }
}

fn convert_proto_read_event(
    event: streams::read_resp::ReadEvent,
) -> ResolvedEvent {
    let commit_position =
        if let Some(pos_alt) = event.position {
            match pos_alt {
                streams::read_resp::read_event::Position::CommitPosition(pos) => Some(pos),
                streams::read_resp::read_event::Position::NoPosition(_) => None,
            }
        } else {
            None
        };

    ResolvedEvent {
        event: event.event.map(convert_proto_recorded_event),
        link: event.link.map(convert_proto_recorded_event),
        commit_position,
    }
}

/// Command that sends events to a given stream.
pub struct WriteEvents {
    client: StreamsClient<Channel>,
    stream: String,
    require_master: bool,
    version: ExpectedVersion,
    creds: Option<types::Credentials>,
}

impl WriteEvents {
    pub(crate) fn new(client: StreamsClient<Channel>, stream: String) -> Self
    {
        WriteEvents {
            client,
            stream,
            require_master: false,
            version: ExpectedVersion::Any,
            creds: None,
        }
    }

    /// Asks the server receiving the command to be the master of the cluster
    /// in order to perform the write. Default: `false`.
    pub fn require_master(self, require_master: bool) -> Self {
        WriteEvents {
            require_master,
            ..self
        }
    }

    /// Asks the server to check that the stream receiving the event is at
    /// the given expected version. Default: `types::ExpectedVersion::Any`.
    pub fn expected_version(self, version: ExpectedVersion) -> Self {
        WriteEvents { version, ..self }
    }

    /// Performs the command with the given credentials.
    pub fn credentials(self, creds: types::Credentials) -> Self {
        WriteEvents {
            creds: Some(creds),
            ..self
        }
    }

    /// Sends asynchronously the write command to the server.
    pub async fn send<S>(mut self, stream: S)
        -> Result<WriteResult, tonic::Status>
    where
        S: Stream<Item=EventData> + Send + Sync + 'static,
    {
        use stream::StreamExt;
        use streams::AppendReq;
        use streams::append_req::{self, Content};
        use crate::es6::commands::streams::append_resp::{CurrentRevisionOption, PositionOption};

        let header = Content::Options(append_req::Options{
            stream_name: self.stream,
            expected_stream_revision: Some(convert_expected_version(self.version)),
        });
        let header = AppendReq {
            content: Some(header),
        };
        let header = stream::once(async move { header });
        let events = stream.map(convert_event_data);
        let payload = header.chain(events);

        let resp = self.client
            .append(Request::new(payload))
            .await?
            .into_inner();

        let next_expected_version =
            match resp.current_revision_option.unwrap() {
                CurrentRevisionOption::CurrentRevision(rev) => rev,
                CurrentRevisionOption::NoStream(_) => 0,
            };

        let position =
            match resp.position_option.unwrap() {
                PositionOption::Position(pos) => {
                    Position {
                        commit: pos.commit_position,
                        prepare: pos.prepare_position,
                    }
                }

                PositionOption::Empty(_) => {
                    Position::start()
                }
            };

        let write_result = WriteResult {
            next_expected_version,
            position,
        };

        Ok(write_result)
    }
}

/// A command that reads several events from a stream. It can read events
/// forward or backward.
pub struct ReadStreamEvents {
    client: StreamsClient<Channel>,
    stream: String,
    max_count: i32,
    revision: Revision<u64>,
    require_master: bool,
    resolve_link_tos: bool,
    direction: types::ReadDirection,
    creds: Option<types::Credentials>,
}

impl ReadStreamEvents {
    pub(crate) fn new(client: StreamsClient<Channel>, stream: String) -> Self
    {
        ReadStreamEvents {
            client,
            stream,
            max_count: 500,
            revision: Revision::Start,
            require_master: false,
            resolve_link_tos: false,
            direction: types::ReadDirection::Forward,
            creds: None,
        }
    }

    /// Asks the command to read forward (toward the end of the stream).
    /// That's the default behavior.
    pub fn forward(self) -> Self {
        self.set_direction(types::ReadDirection::Forward)
    }

    /// Asks the command to read backward (toward the begining of the stream).
    pub fn backward(self) -> Self {
        self.set_direction(types::ReadDirection::Backward)
    }

    fn set_direction(self, direction: types::ReadDirection) -> Self {
        ReadStreamEvents { direction, ..self }
    }

    /// Performs the command with the given credentials.
    pub fn credentials(self, value: types::Credentials) -> Self {
        ReadStreamEvents {
            creds: Some(value),
            ..self
        }
    }

    /// Performs the command with the given credentials.
    pub fn set_credentials(self, creds: Option<types::Credentials>) -> Self {
        ReadStreamEvents { creds, ..self }
    }

    /// Max batch size.
    pub fn max_count(self, max_count: i32) -> Self {
        ReadStreamEvents { max_count, ..self }
    }

    /// Starts the read at the given event number. By default, it starts at
    /// 0.
    pub fn start_from(self, start: u64) -> Self {
        ReadStreamEvents { revision: Revision::Exact(start), ..self }
    }

    /// Starts the read from the beginning of the stream. It also set the read
    /// direction to `Forward`.
    pub fn start_from_beginning(self) -> Self {
        ReadStreamEvents {
            revision: Revision::Start,
            direction: types::ReadDirection::Forward,
            ..self
        }
    }

    /// Starts the read from the end of the stream. It also set the read
    /// direction to `Backward`.
    pub fn start_from_end_of_stream(self) -> Self {
        ReadStreamEvents {
            revision: Revision::End,
            direction: types::ReadDirection::Backward,
            ..self
        }
    }

    /// Asks the server receiving the command to be the master of the cluster
    /// in order to perform the write. Default: `false`.
    pub fn require_master(self, require_master: bool) -> Self {
        ReadStreamEvents {
            require_master,
            ..self
        }
    }

    /// When using projections, you can have links placed into another stream.
    /// If you set `true`, the server will resolve those links and will return
    /// the event that the link points to. Default: [NoResolution](../types/enum.LinkTos.html).
    pub fn resolve_link_tos(self, tos: types::LinkTos) -> Self {
        let resolve_link_tos = tos.raw_resolve_lnk_tos();

        ReadStreamEvents {
            resolve_link_tos,
            ..self
        }
    }

    /// Sends asynchronously the read command to the server.
    pub async fn execute(
        mut self,
        count: u64,
    ) -> Result<Box<dyn Stream<Item=Result<ResolvedEvent, tonic::Status>>>, tonic::Status> {
        use futures::stream::TryStreamExt;
        use streams::read_req::{Empty, Options};
        use streams::read_req::options::{self, StreamOption, StreamOptions};
        use streams::read_req::options::stream_options::{RevisionOption};

        let read_direction = match self.direction {
            types::ReadDirection::Forward => 0,
            types::ReadDirection::Backward => 1,
        };

        let revision_option = match self.revision {
            Revision::Exact(rev) => RevisionOption::Revision(rev),
            Revision::Start => RevisionOption::Start(Empty{}),
            Revision::End => RevisionOption::End(Empty{}),
        };

        let stream_options = StreamOptions {
            stream_name: self.stream,
            revision_option: Some(revision_option),
        };

        let uuid_option = options::UuidOption {
            content: Some(options::uuid_option::Content::String(Empty{}))
        };

        let options = Options {
            stream_option: Some(StreamOption::Stream(stream_options)),
            resolve_links: self.resolve_link_tos,
            filter_option: Some(options::FilterOption::NoFilter(Empty{})),
            count_option: Some(options::CountOption::Count(count)),
            uuid_option: Some(uuid_option),
            read_direction,
        };

        let req = streams::ReadReq {
            options: Some(options),
        };

        let req = Request::new(req);

        let stream = self.client.read(req)
            .await?
            .into_inner();

        // TODO - I'm not so sure about that unwrap here.
        let stream = stream.map_ok(|resp| convert_proto_read_event(resp.event.unwrap()));

        Ok(Box::new(stream))
    }
}

fn lift_to_stream(
    evts: Vec<types::ResolvedEvent>,
) -> impl Stream<Item = Result<types::ResolvedEvent, OperationError>> {
    use futures::stream;

    let evts = evts
        .into_iter()
        .map(Ok::<types::ResolvedEvent, OperationError>);

    stream::iter(evts)
}

/// Like `ReadStreamEvents` but specialized to system stream '$all'.
pub struct ReadAllEvents {
    client: StreamsClient<Channel>,
    max_count: i32,
    revision: Revision<Position>,
    require_master: bool,
    resolve_link_tos: bool,
    direction: types::ReadDirection,
    creds: Option<types::Credentials>,
}

impl ReadAllEvents {
    pub(crate) fn new(client: StreamsClient<Channel>) -> ReadAllEvents {
        ReadAllEvents {
            client,
            max_count: 500,
            revision: Revision::Start,
            require_master: false,
            resolve_link_tos: false,
            direction: types::ReadDirection::Forward,
            creds: None,
        }
    }

    /// Asks the command to read forward (toward the end of the stream).
    /// That's the default behavior.
    pub fn forward(self) -> Self {
        self.set_direction(types::ReadDirection::Forward)
    }

    /// Asks the command to read backward (toward the begining of the stream).
    pub fn backward(self) -> Self {
        self.set_direction(types::ReadDirection::Backward)
    }

    fn set_direction(self, direction: types::ReadDirection) -> Self {
        ReadAllEvents { direction, ..self }
    }

    /// Performs the command with the given credentials.
    pub fn credentials(self, value: types::Credentials) -> Self {
        ReadAllEvents {
            creds: Some(value),
            ..self
        }
    }

    /// Max batch size.
    pub fn max_count(self, max_count: i32) -> Self {
        ReadAllEvents { max_count, ..self }
    }

    /// Starts the read ot the given event number. By default, it starts at
    /// `types::Position::start`.
    pub fn start_from(self, start: Position) -> Self {
        let revision = Revision::Exact(start);
        ReadAllEvents { revision, ..self }
    }

    /// Starts the read from the beginning of the stream. It also set the read
    /// direction to `Forward`.
    pub fn start_from_beginning(self) -> Self {
        let revision = Revision::Start;
        let direction = types::ReadDirection::Forward;

        ReadAllEvents {
            revision,
            direction,
            ..self
        }
    }

    /// Starts the read from the end of the stream. It also set the read
    /// direction to `Backward`.
    pub fn start_from_end_of_stream(self) -> Self {
        let revision = Revision::End;
        let direction = types::ReadDirection::Backward;

        ReadAllEvents {
            revision,
            direction,
            ..self
        }
    }

    /// Asks the server receiving the command to be the master of the cluster
    /// in order to perform the write. Default: `false`.
    pub fn require_master(self, require_master: bool) -> Self {
        ReadAllEvents {
            require_master,
            ..self
        }
    }

    /// When using projections, you can have links placed into another stream.
    /// If you set `true`, the server will resolve those links and will return
    /// the event that the link points to. Default: [NoResolution](../types/enum.LinkTos.html).
    pub fn resolve_link_tos(self, tos: types::LinkTos) -> Self {
        let resolve_link_tos = tos.raw_resolve_lnk_tos();

        ReadAllEvents {
            resolve_link_tos,
            ..self
        }
    }

    /// Sends asynchronously the read command to the server.
    pub async fn execute(
        mut self,
        count: u64
    ) -> Result<Box<dyn Stream<Item=Result<ResolvedEvent, tonic::Status>>>, tonic::Status>  {
        use futures::stream::TryStreamExt;
        use streams::read_req::{Empty, Options};
        use streams::read_req::options::{self, StreamOption, AllOptions};
        use streams::read_req::options::all_options::AllOption;

        let read_direction = match self.direction {
            types::ReadDirection::Forward => 0,
            types::ReadDirection::Backward => 1,
        };

        let all_option = match self.revision {
            Revision::Exact(pos) => {
                let pos = options::Position {
                    commit_position: pos.commit,
                    prepare_position: pos.prepare,
                };

                AllOption::Position(pos)
            }

            Revision::Start => AllOption::Start(Empty{}),
            Revision::End => AllOption::End(Empty{}),
        };

        let stream_options = AllOptions {
            all_option: Some(all_option),
        };

        let uuid_option = options::UuidOption {
            content: Some(options::uuid_option::Content::String(Empty{}))
        };

        let options = Options {
            stream_option: Some(StreamOption::All(stream_options)),
            resolve_links: self.resolve_link_tos,
            filter_option: Some(options::FilterOption::NoFilter(Empty{})),
            count_option: Some(options::CountOption::Count(count)),
            uuid_option: Some(uuid_option),
            read_direction,
        };

        let req = streams::ReadReq {
            options: Some(options),
        };

        let req = Request::new(req);

        let stream = self.client.read(req)
            .await?
            .into_inner();

        // TODO - I'm not so sure about that unwrap here.
        let stream = stream.map_ok(|resp| convert_proto_read_event(resp.event.unwrap()));

        Ok(Box::new(stream))
    }
}

/// Command that deletes a stream. More information on [Deleting stream and events].
///
/// [Deleting stream and events]: https://eventstore.org/docs/server/deleting-streams-and-events/index.html
pub struct DeleteStream {
    client: StreamsClient<Channel>,
    stream: String,
    require_master: bool,
    version: ExpectedVersion,
    creds: Option<types::Credentials>,
    hard_delete: bool,
}

impl DeleteStream {
    pub(crate) fn new(client: StreamsClient<Channel>, stream: String) -> DeleteStream
    {
        DeleteStream {
            client,
            stream,
            require_master: false,
            hard_delete: false,
            version: ExpectedVersion::Any,
            creds: None,
        }
    }

    /// Asks the server receiving the command to be the master of the cluster
    /// in order to perform the write. Default: `false`.
    pub fn require_master(self, require_master: bool) -> Self {
        DeleteStream {
            require_master,
            ..self
        }
    }

    /// Asks the server to check that the stream receiving the event is at
    /// the given expected version. Default: `types::ExpectedVersion::Any`.
    pub fn expected_version(self, version: ExpectedVersion) -> Self {
        DeleteStream { version, ..self }
    }

    /// Performs the command with the given credentials.
    pub fn credentials(self, value: types::Credentials) -> Self {
        DeleteStream {
            creds: Some(value),
            ..self
        }
    }

    /// Makes use of Truncate before. When a stream is deleted, its Truncate
    /// before is set to the streams current last event number. When a soft
    /// deleted stream is read, the read will return a StreamNotFound. After
    /// deleting the stream, you are able to write to it again, continuing from
    /// where it left off.
    ///
    /// That is the default behavior.
    pub fn soft_delete(self) -> Self {
        DeleteStream {
            hard_delete: false,
            ..self
        }
    }

    /// A hard delete writes a tombstone event to the stream, permanently
    /// deleting it. The stream cannot be recreated or written to again.
    /// Tombstone events are written with the event type '$streamDeleted'. When
    /// a hard deleted stream is read, the read will return a StreamDeleted.
    pub fn hard_delete(self) -> Self {
        DeleteStream {
            hard_delete: true,
            ..self
        }
    }

    /// Sends asynchronously the delete command to the server.
    pub async fn execute(mut self) -> Result<Option<Position>, tonic::Status> {
        if self.hard_delete {
            use streams::tombstone_req::{Options, Empty};
            use streams::tombstone_req::options::ExpectedStreamRevision;
            use streams::tombstone_resp::PositionOption;

            let expected_stream_revision = match self.version {
                ExpectedVersion::Any => ExpectedStreamRevision::Any(Empty{}),
                ExpectedVersion::NoStream => ExpectedStreamRevision::NoStream(Empty{}),
                ExpectedVersion::StreamExists => ExpectedStreamRevision::StreamExists(Empty{}),
                ExpectedVersion::Exact(rev) => ExpectedStreamRevision::Revision(rev),
            };

            let expected_stream_revision = Some(expected_stream_revision);

            let options = Options {
                stream_name: self.stream,
                expected_stream_revision,
            };

            let req = Request::new(streams::TombstoneReq {
                options: Some(options),
            });

            let result = self.client.tombstone(req)
                .await?
                .into_inner();

            if let Some(opts) = result.position_option {
                match opts {
                    PositionOption::Position(pos) => {
                        let pos = Position {
                            commit: pos.commit_position,
                            prepare: pos.prepare_position,
                        };

                        Ok(Some(pos))
                    }

                    PositionOption::Empty(_) => Ok(None),
                }
            } else {
                Ok(None)
            }
        } else {
            use streams::delete_req::{Options, Empty};
            use streams::delete_req::options::ExpectedStreamRevision;
            use streams::delete_resp::PositionOption;

            let expected_stream_revision = match self.version {
                ExpectedVersion::Any => ExpectedStreamRevision::Any(Empty{}),
                ExpectedVersion::NoStream => ExpectedStreamRevision::NoStream(Empty{}),
                ExpectedVersion::StreamExists => ExpectedStreamRevision::StreamExists(Empty{}),
                ExpectedVersion::Exact(rev) => ExpectedStreamRevision::Revision(rev),
            };

            let expected_stream_revision = Some(expected_stream_revision);

            let options = Options {
                stream_name: self.stream,
                expected_stream_revision,
            };

            let req = Request::new(streams::DeleteReq {
                options: Some(options),
            });

            let result = self.client.delete(req)
                .await?
                .into_inner();

            if let Some(opts) = result.position_option {
                match opts {
                    PositionOption::Position(pos) => {
                        let pos = Position {
                            commit: pos.commit_position,
                            prepare: pos.prepare_position,
                        };

                        Ok(Some(pos))
                    }

                    PositionOption::Empty(_) => Ok(None),
                }
            } else {
                Ok(None)
            }
        }
    }
}

/// Subscribes to a given stream. This kind of subscription specifies a
/// starting point (by default, the beginning of a stream). For a regular
/// stream, that starting point will be an event number. For the system
/// stream `$all`, it will be a position in the transaction file
/// (see `subscribe_to_all_from`). This subscription will fetch every event
/// until the end of the stream, then will dispatch subsequently written
/// events.
///
/// For example, if a starting point of 50 is specified when a stream has
/// 100 events in it, the subscriber can expect to see events 51 through
/// 100, and then any events subsequenttly written events until such time
/// as the subscription is dropped or closed.
///
/// * Notes
/// Catchup subscription are resilient to connection drops.
/// Basically, if the connection drops. The command will restart its
/// catching up phase from the begining and then emit a new volatile
/// subscription request.
///
/// All this process happens without the user has to do anything.
pub struct RegularCatchupSubscribe {
    client: StreamsClient<Channel>,
    stream_id: String,
    resolve_link_tos: bool,
    require_master: bool,
    batch_size: i32,
    revision: Option<u64>,
    creds_opt: Option<types::Credentials>,
}

impl RegularCatchupSubscribe {
    pub(crate) fn new(client: StreamsClient<Channel>, stream_id: String) -> RegularCatchupSubscribe {
        RegularCatchupSubscribe {
            client,
            stream_id,
            resolve_link_tos: false,
            require_master: false,
            batch_size: 500,
            revision: None,
            creds_opt: None,
        }
    }

    /// When using projections, you can have links placed into another stream.
    /// If you set `true`, the server will resolve those links and will return
    /// the event that the link points to. Default: [NoResolution](../types/enum.LinkTos.html).
    pub fn resolve_link_tos(self, tos: types::LinkTos) -> Self {
        let resolve_link_tos = tos.raw_resolve_lnk_tos();

        RegularCatchupSubscribe {
            resolve_link_tos,
            ..self
        }
    }

    /// Asks the server receiving the command to be the master of the cluster
    /// in order to perform the write. Default: `false`.
    pub fn require_master(self, require_master: bool) -> Self {
        RegularCatchupSubscribe {
            require_master,
            ..self
        }
    }

    /// For example, if a starting point of 50 is specified when a stream has
    /// 100 events in it, the subscriber can expect to see events 51 through
    /// 100, and then any events subsequenttly written events until such time
    /// as the subscription is dropped or closed.
    ///
    /// By default, it will start from the event number 0.
    pub fn start_position(self, start_pos: u64) -> Self {
        let revision = Some(start_pos);
        RegularCatchupSubscribe { revision, ..self }
    }

    /// Performs the command with the given credentials.
    pub fn credentials(self, creds: types::Credentials) -> Self {
        RegularCatchupSubscribe {
            creds_opt: Some(creds),
            ..self
        }
    }

    /// Preforms the catching up phase of the subscription asynchronously. When
    /// it will reach the head of stream, the command will emit a volatile
    /// subscription request.
    pub async fn execute(
        mut self,
    ) -> Result<Box<dyn Stream<Item=Result<ResolvedEvent, tonic::Status>>>, tonic::Status> {
        use futures::stream::TryStreamExt;
        use streams::read_req::{Empty, Options};
        use streams::read_req::options::{self, StreamOption, StreamOptions, SubscriptionOptions};
        use streams::read_req::options::stream_options::RevisionOption;

        let read_direction = 0; // <- Going forward.

        let revision_option = match self.revision {
            Some(rev) => RevisionOption::Revision(rev),
            None => RevisionOption::Start(Empty{}),
        };

        let stream_options = StreamOptions {
            stream_name: self.stream_id,
            revision_option: Some(revision_option),
        };

        let uuid_option = options::UuidOption {
            content: Some(options::uuid_option::Content::String(Empty{}))
        };

        let options = Options {
            stream_option: Some(StreamOption::Stream(stream_options)),
            resolve_links: self.resolve_link_tos,
            filter_option: Some(options::FilterOption::NoFilter(Empty{})),
            count_option: Some(options::CountOption::Subscription(SubscriptionOptions{})),
            uuid_option: Some(uuid_option),
            read_direction,
        };

        let req = streams::ReadReq {
            options: Some(options),
        };

        let req = Request::new(req);

        let stream = self.client.read(req)
            .await?
            .into_inner();

        // TODO - I'm not so sure about that unwrap here.
        let stream = stream.map_ok(|resp| convert_proto_read_event(resp.event.unwrap()));

        Ok(Box::new(stream))
    }
}

/// Like `RegularCatchupSubscribe` but specific to the system stream '$all'.
pub struct AllCatchupSubscribe {
    client: StreamsClient<Channel>,
    resolve_link_tos: bool,
    require_master: bool,
    batch_size: i32,
    revision: Option<Position>,
    creds_opt: Option<types::Credentials>,
}

impl AllCatchupSubscribe {
    pub(crate) fn new(client: StreamsClient<Channel>) -> AllCatchupSubscribe {
        AllCatchupSubscribe {
            client,
            resolve_link_tos: false,
            require_master: false,
            batch_size: 500,
            revision: None,
            creds_opt: None,
        }
    }

    /// When using projections, you can have links placed into another stream.
    /// If you set `true`, the server will resolve those links and will return
    /// the event that the link points to. Default: [NoResolution](../types/enum.LinkTos.html).
    pub fn resolve_link_tos(self, tos: types::LinkTos) -> Self {
        let resolve_link_tos = tos.raw_resolve_lnk_tos();

        AllCatchupSubscribe {
            resolve_link_tos,
            ..self
        }
    }

    /// Asks the server receiving the command to be the master of the cluster
    /// in order to perform the write. Default: `false`.
    pub fn require_master(self, require_master: bool) -> Self {
        AllCatchupSubscribe {
            require_master,
            ..self
        }
    }

    /// Starting point in the transaction journal log. By default, it will start at
    /// `Revision::Start`.
    pub fn start_position(self, start_pos: Position) -> Self {
        let revision = Some(start_pos);

        AllCatchupSubscribe { revision, ..self }
    }

    /// Performs the command with the given credentials.
    pub fn credentials(self, creds: types::Credentials) -> Self {
        AllCatchupSubscribe {
            creds_opt: Some(creds),
            ..self
        }
    }

    /// Preforms the catching up phase of the subscription asynchronously. When
    /// it will reach the head of stream, the command will emit a volatile
    /// subscription request.
    pub async fn execute(
        mut self,
    ) -> Result<Box<dyn Stream<Item=Result<ResolvedEvent, tonic::Status>>>, tonic::Status> {
        use futures::stream::TryStreamExt;
        use streams::read_req::{Empty, Options};
        use streams::read_req::options::{self, StreamOption, StreamOptions, SubscriptionOptions, AllOptions};
        use streams::read_req::options::stream_options::RevisionOption;
        use streams::read_req::options::all_options::AllOption;

        let read_direction = 0; // <- Going forward.

        let all_option = match self.revision {
            Some(pos) => {
                let pos = options::Position {
                    commit_position: pos.commit,
                    prepare_position: pos.prepare,
                };

                AllOption::Position(pos)
            }

            None => AllOption::Start(Empty{}),
        };

        let stream_options = AllOptions {
            all_option: Some(all_option),
        };


        let uuid_option = options::UuidOption {
            content: Some(options::uuid_option::Content::String(Empty{}))
        };

        let options = Options {
            stream_option: Some(StreamOption::All(stream_options)),
            resolve_links: self.resolve_link_tos,
            filter_option: Some(options::FilterOption::NoFilter(Empty{})),
            count_option: Some(options::CountOption::Subscription(SubscriptionOptions{})),
            uuid_option: Some(uuid_option),
            read_direction,
        };

        let req = streams::ReadReq {
            options: Some(options),
        };

        let req = Request::new(req);

        let stream = self.client.read(req)
            .await?
            .into_inner();

        // TODO - I'm not so sure about that unwrap here.
        let stream = stream.map_ok(|resp| convert_proto_read_event(resp.event.unwrap()));

        Ok(Box::new(stream))
    }
}

/// A command that creates a persistent subscription for a given group.
pub struct CreatePersistentSubscription {
    client: PersistentSubscriptionsClient<Channel>,
    stream_id: String,
    group_name: String,
    sub_settings: PersistentSubscriptionSettings,
    creds: Option<types::Credentials>,
}

impl CreatePersistentSubscription {
    pub(crate) fn new(
        client: PersistentSubscriptionsClient<Channel>,
        stream_id: String,
        group_name: String,
    ) -> CreatePersistentSubscription {
        CreatePersistentSubscription {
            client,
            stream_id,
            group_name,
            creds: None,
            sub_settings: PersistentSubscriptionSettings::default(),
        }
    }

    /// Performs the command with the given credentials.
    pub fn credentials(self, creds: types::Credentials) -> Self {
        CreatePersistentSubscription {
            creds: Some(creds),
            ..self
        }
    }

    /// Creates a persistent subscription based on the given
    /// `types::PersistentSubscriptionSettings`.
    pub fn settings(self, sub_settings: PersistentSubscriptionSettings) -> Self {
        CreatePersistentSubscription {
            sub_settings,
            ..self
        }
    }

    /// Sends the persistent subscription creation command asynchronously to
    /// the server.
    pub async fn execute(mut self) -> Result<(), tonic::Status> {
        use persistent::CreateReq;
        use persistent::create_req::Options;

        let settings = convert_settings_create(self.sub_settings);
        let options = Options {
            stream_name: self.stream_id,
            group_name: self.group_name,
            settings: Some(settings),
        };

        let req = CreateReq {
            options: Some(options),
        };

        self.client.create(Request::new(req)).await?;

        Ok(())
    }
}

/// Command that updates an already existing subscription's settings.
pub struct UpdatePersistentSubscription {
    client: PersistentSubscriptionsClient<Channel>,
    stream_id: String,
    group_name: String,
    sub_settings: PersistentSubscriptionSettings,
    creds: Option<types::Credentials>,
}

impl UpdatePersistentSubscription {
    pub(crate) fn new(
        client: PersistentSubscriptionsClient<Channel>,
        stream_id: String,
        group_name: String,
    ) -> UpdatePersistentSubscription {
        UpdatePersistentSubscription {
            client,
            stream_id,
            group_name,
            creds: None,
            sub_settings: PersistentSubscriptionSettings::default(),
        }
    }

    /// Performs the command with the given credentials.
    pub fn credentials(self, creds: types::Credentials) -> Self {
        UpdatePersistentSubscription {
            creds: Some(creds),
            ..self
        }
    }

    /// Updates a persistent subscription using the given
    /// `types::PersistentSubscriptionSettings`.
    pub fn settings(self, sub_settings: PersistentSubscriptionSettings) -> Self {
        UpdatePersistentSubscription {
            sub_settings,
            ..self
        }
    }

    /// Sends the persistent subscription update command asynchronously to
    /// the server.
    pub async fn execute(mut self) -> Result<(), tonic::Status> {
        use persistent::UpdateReq;
        use persistent::update_req::Options;

        let settings = convert_settings_update(self.sub_settings);
        let options = Options {
            stream_name: self.stream_id,
            group_name: self.group_name,
            settings: Some(settings),
        };

        let req = UpdateReq {
            options: Some(options),
        };

        self.client.update(Request::new(req)).await?;

        Ok(())
    }
}

/// Command that  deletes a persistent subscription.
pub struct DeletePersistentSubscription {
    client: PersistentSubscriptionsClient<Channel>,
    stream_id: String,
    group_name: String,
    creds: Option<types::Credentials>,
}

impl DeletePersistentSubscription {
    pub(crate) fn new(
        client: PersistentSubscriptionsClient<Channel>,
        stream_id: String,
        group_name: String,
    ) -> DeletePersistentSubscription {
        DeletePersistentSubscription {
            client,
            stream_id,
            group_name,
            creds: None,
        }
    }

    /// Performs the command with the given credentials.
    pub fn credentials(self, creds: types::Credentials) -> Self {
        DeletePersistentSubscription {
            creds: Some(creds),
            ..self
        }
    }

    /// Sends the persistent subscription deletion command asynchronously to
    /// the server.
    pub async fn execute(mut self) -> Result<(), tonic::Status> {
        use persistent::delete_req::Options;

        let options = Options {
            stream_name: self.stream_id,
            group_name: self.group_name,
        };

        let req = persistent::DeleteReq {
            options: Some(options),
        };

        self.client.delete(Request::new(req)).await?;

        Ok(())
    }
}

/// A subscription model where the server remembers the state of the
/// consumption of a stream. This allows for many different modes of operations
/// compared to a regular subscription where the client hols the subscription
/// state.
pub struct ConnectToPersistentSubscription {
    client: PersistentSubscriptionsClient<Channel>,
    stream_id: String,
    group_name: String,
    batch_size: i32,
    creds: Option<types::Credentials>,
}

impl ConnectToPersistentSubscription {
    pub(crate) fn new(
        client: PersistentSubscriptionsClient<Channel>,
        stream_id: String,
        group_name: String,
    ) -> ConnectToPersistentSubscription {
        ConnectToPersistentSubscription {
            client,
            stream_id,
            group_name,
            batch_size: 10,
            creds: None,
        }
    }

    /// Performs the command with the given credentials.
    pub fn credentials(self, creds: types::Credentials) -> Self {
        ConnectToPersistentSubscription {
            creds: Some(creds),
            ..self
        }
    }

    /// The buffer size to use  for the persistent subscription.
    pub fn batch_size(self, batch_size: i32) -> Self {
        ConnectToPersistentSubscription { batch_size, ..self }
    }

    /// Sends the persistent subscription connection request to the server
    /// asynchronously even if the subscription is available right away.
    pub async fn execute(mut self) -> Result<(), tonic::Status> {
        use futures::stream::once;
        use persistent::ReadReq;
        use persistent::read_req::{self, Options, Empty};
        use persistent::read_req::options::{self, UuidOption};

        let uuid_option = UuidOption {
            content: Some(options::uuid_option::Content::String(Empty{})),
        };

        let options = Options {
            stream_name: self.stream_id,
            group_name: self.group_name,
            buffer_size: self.batch_size,
            uuid_option: Some(uuid_option),
        };

        let req = ReadReq {
            content: Some(read_req::Content::Options(options)),
        };

        let payload = once(async { req });
        let req = Request::new(payload);

        self.client.read(req).await?;

        unimplemented!()
    }
}
