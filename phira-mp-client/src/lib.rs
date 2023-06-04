use anyhow::{Context, Error, Result};
use phira_mp_common::{
    ClientCommand, ClientRoomState, JudgeEvent, Message, RoomState, ServerCommand, Stream,
    TouchFrame, HEARTBEAT_INTERVAL, HEARTBEAT_TIMEOUT,
};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{
    net::TcpStream,
    sync::{oneshot, Mutex, MutexGuard, Notify, RwLock},
    task::JoinHandle,
    time,
};
use tracing::{error, trace, warn};
use uuid::Uuid;

type Callback<T> = Mutex<Option<oneshot::Sender<T>>>;
type RCallback<T, E = String> = Mutex<Option<oneshot::Sender<Result<T, E>>>>;

pub const TIMEOUT: Duration = Duration::from_secs(7);

struct State {
    delay: Mutex<Option<Duration>>,
    ping_notify: Notify,

    room: RwLock<Option<ClientRoomState>>,

    cb_authorize: RCallback<Option<ClientRoomState>>,
    cb_chat: RCallback<()>,
    cb_create_room: RCallback<Uuid>,
    cb_join_room: RCallback<RoomState>,
    cb_leave_room: RCallback<()>,
    cb_select_chart: RCallback<()>,
    cb_request_start: RCallback<()>,
    cb_ready: RCallback<()>,
    cb_cancel_ready: RCallback<()>,
    cb_played: RCallback<()>,

    touch_frames: Mutex<VecDeque<TouchFrame>>,
    judges: Mutex<VecDeque<JudgeEvent>>,
    messages: Mutex<Vec<Message>>,
}

pub struct Client {
    state: Arc<State>,

    stream: Arc<Stream<ClientCommand, ServerCommand>>,

    ping_fail_count: Arc<AtomicU8>,
    ping_task_handle: JoinHandle<()>,
}

impl Client {
    pub async fn new(stream: TcpStream) -> Result<Self> {
        stream.set_nodelay(true)?;

        let state = Arc::new(State {
            delay: Mutex::default(),
            ping_notify: Notify::new(),

            room: RwLock::default(),

            cb_authorize: Callback::default(),
            cb_chat: Callback::default(),
            cb_create_room: Callback::default(),
            cb_join_room: Callback::default(),
            cb_leave_room: Callback::default(),
            cb_select_chart: Callback::default(),
            cb_request_start: Callback::default(),
            cb_ready: Callback::default(),
            cb_cancel_ready: Callback::default(),
            cb_played: Callback::default(),

            touch_frames: Mutex::default(),
            judges: Mutex::default(),
            messages: Mutex::default(),
        });
        let stream = Arc::new(
            Stream::new(
                Some(1),
                stream,
                Box::new({
                    let state = Arc::clone(&state);
                    move |_send_tx, cmd| process(Arc::clone(&state), cmd)
                }),
            )
            .await?,
        );

        let ping_fail_count = Arc::new(AtomicU8::default());
        let ping_task_handle = tokio::spawn({
            let ping_fail_count = Arc::clone(&ping_fail_count);
            let state = Arc::clone(&state);
            let stream = Arc::clone(&stream);
            async move {
                loop {
                    time::sleep(HEARTBEAT_INTERVAL).await;

                    let start = Instant::now();
                    if let Err(err) = stream.send(ClientCommand::Ping).await {
                        error!("failed to send heartbeat: {err:?}");
                    } else if time::timeout(HEARTBEAT_TIMEOUT, state.ping_notify.notified())
                        .await
                        .is_err()
                    {
                        warn!("heartbeat timeout");
                        ping_fail_count.fetch_add(1, Ordering::Relaxed);
                    } else {
                        ping_fail_count.store(0, Ordering::SeqCst);
                    }
                    let delay = start.elapsed();
                    *state.delay.lock().await = Some(delay);
                    trace!("sent heartbeat, delay: {delay:?}");
                }
            }
        });

        Ok(Self {
            state,

            stream,

            ping_fail_count,
            ping_task_handle,
        })
    }

    pub fn blocking_take_messages(&self) -> Vec<Message> {
        self.state.messages.blocking_lock().drain(..).collect()
    }

    pub fn blocking_room_id(&self) -> Option<Uuid> {
        self.state.room.blocking_read().as_ref().map(|it| it.id)
    }

    pub fn blocking_room_state(&self) -> Option<RoomState> {
        self.state.room.blocking_read().as_ref().map(|it| it.state)
    }

    pub fn blocking_is_host(&self) -> Option<bool> {
        self.state
            .room
            .blocking_read()
            .as_ref()
            .map(|it| it.is_host)
    }

    pub fn blocking_is_ready(&self) -> Option<bool> {
        self.state
            .room
            .blocking_read()
            .as_ref()
            .map(|it| it.is_ready)
    }

    pub async fn ping(&self) -> Result<Duration> {
        let start = Instant::now();
        self.stream.send(ClientCommand::Ping).await?;
        time::timeout(HEARTBEAT_TIMEOUT, self.state.ping_notify.notified())
            .await
            .context("heartbeat timeout")?;
        let delay = start.elapsed();
        *self.state.delay.lock().await = Some(delay);
        Ok(delay)
    }

    pub fn delay(&self) -> Option<Duration> {
        *self.state.delay.blocking_lock()
    }

    async fn rcall<R>(&self, payload: ClientCommand, cb: &RCallback<R>) -> Result<R> {
        self.stream.send(payload).await?;
        let (tx, rx) = oneshot::channel();
        *cb.lock().await = Some(tx);
        time::timeout(TIMEOUT, rx)
            .await
            .context("timeout")??
            .map_err(Error::msg)
    }

    #[inline]
    pub async fn authorize(&self, token: impl Into<String>) -> Result<()> {
        let room = self
            .rcall(
                ClientCommand::Authorize {
                    token: token.into().try_into()?,
                },
                &self.state.cb_authorize,
            )
            .await?;
        *self.state.room.write().await = room;
        Ok(())
    }

    #[inline]
    pub async fn chat(&self, message: String) -> Result<()> {
        self.rcall(
            ClientCommand::Chat {
                message: message.try_into()?,
            },
            &self.state.cb_chat,
        )
        .await
    }

    #[inline]
    pub async fn create_room(&self) -> Result<Uuid> {
        let id = self
            .rcall(ClientCommand::CreateRoom, &self.state.cb_create_room)
            .await?;
        *self.state.room.write().await = Some(ClientRoomState {
            id,
            state: RoomState::default(),
            is_host: true,
            is_ready: false,
        });
        Ok(id)
    }

    #[inline]
    pub async fn join_room(&self, id: Uuid) -> Result<()> {
        let state = self
            .rcall(ClientCommand::JoinRoom { id }, &self.state.cb_join_room)
            .await?;
        *self.state.room.write().await = Some(ClientRoomState {
            id,
            state,
            is_host: false,
            is_ready: false,
        });
        Ok(())
    }

    #[inline]
    pub async fn leave_room(&self) -> Result<()> {
        self.rcall(ClientCommand::LeaveRoom, &self.state.cb_leave_room)
            .await?;
        *self.state.room.write().await = None;
        Ok(())
    }

    #[inline]
    pub async fn select_chart(&self, id: i32) -> Result<()> {
        self.rcall(
            ClientCommand::SelectChart { id },
            &self.state.cb_select_chart,
        )
        .await
    }

    #[inline]
    pub async fn request_start(&self) -> Result<()> {
        self.rcall(ClientCommand::RequestStart, &self.state.cb_request_start)
            .await?;
        self.state.room.write().await.as_mut().unwrap().is_ready = true;
        Ok(())
    }

    #[inline]
    pub async fn ready(&self) -> Result<()> {
        self.rcall(ClientCommand::Ready, &self.state.cb_ready)
            .await?;
        self.state.room.write().await.as_mut().unwrap().is_ready = true;
        Ok(())
    }

    #[inline]
    pub async fn cancel_ready(&self) -> Result<()> {
        self.rcall(ClientCommand::CancelReady, &self.state.cb_cancel_ready)
            .await?;
        self.state.room.write().await.as_mut().unwrap().is_ready = false;
        Ok(())
    }

    #[inline]
    pub async fn played(&self, id: i32) -> Result<()> {
        self.rcall(ClientCommand::Played { id }, &self.state.cb_played)
            .await
    }

    pub fn ping_fail_count(&self) -> u8 {
        self.ping_fail_count.load(Ordering::Relaxed)
    }

    pub async fn send(&self, payload: ClientCommand) -> Result<()> {
        self.stream.send(payload).await
    }

    pub fn blocking_send(&self, payload: ClientCommand) -> Result<()> {
        self.stream.blocking_send(payload)
    }

    pub fn touch_frames(&self) -> MutexGuard<'_, VecDeque<TouchFrame>> {
        self.state.touch_frames.blocking_lock()
    }

    pub fn judge_events(&self) -> MutexGuard<'_, VecDeque<JudgeEvent>> {
        self.state.judges.blocking_lock()
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.ping_task_handle.abort();
    }
}

async fn process(state: Arc<State>, cmd: ServerCommand) {
    async fn cb<T>(cb: &Callback<T>, res: T) {
        let _ = cb.lock().await.take().unwrap().send(res);
    }
    match cmd {
        ServerCommand::Pong => {
            state.ping_notify.notify_one();
        }
        ServerCommand::Authorize(res) => {
            cb(&state.cb_authorize, res).await;
        }
        ServerCommand::Chat(res) => {
            cb(&state.cb_chat, res).await;
        }
        ServerCommand::Touches { frames } => {
            state
                .touch_frames
                .lock()
                .await
                .extend(frames.iter().cloned());
        }
        ServerCommand::Judges { judges } => {
            state.judges.lock().await.extend(judges.iter().cloned());
        }
        ServerCommand::Message(msg) => {
            state.messages.lock().await.push(msg);
        }
        ServerCommand::ChangeState(room) => {
            state.room.write().await.as_mut().unwrap().state = room;
        }
        ServerCommand::ChangeHost(me_is_host) => {
            state.room.write().await.as_mut().unwrap().is_host = me_is_host;
        }

        ServerCommand::CreateRoom(res) => {
            cb(&state.cb_create_room, res).await;
        }
        ServerCommand::JoinRoom(res) => {
            cb(&state.cb_join_room, res).await;
        }
        ServerCommand::LeaveRoom(res) => {
            cb(&state.cb_leave_room, res).await;
        }
        ServerCommand::SelectChart(res) => {
            cb(&state.cb_select_chart, res).await;
        }
        ServerCommand::RequestStart(res) => {
            cb(&state.cb_request_start, res).await;
        }
        ServerCommand::Ready(res) => {
            cb(&state.cb_ready, res).await;
        }
        ServerCommand::CancelReady(res) => {
            cb(&state.cb_cancel_ready, res).await;
        }
        ServerCommand::Played(res) => {
            cb(&state.cb_played, res).await;
        }
        ServerCommand::GameEnd => {}
    }
}