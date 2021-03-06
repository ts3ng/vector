use crate::runtime::Runtime;
use crate::{event::LogEvent, Event};

use futures::{compat::Stream01CompatExt, stream, SinkExt, Stream, StreamExt, TryStreamExt};
use futures01::{
    future, stream as stream01, sync::mpsc, try_ready, Async, Future, Poll, Stream as Stream01,
};
use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
use rand::{distributions::Alphanumeric, thread_rng, Rng};
use std::{
    collections::HashMap,
    convert::Infallible,
    fs::File,
    io::Read,
    iter, mem,
    net::{Shutdown, SocketAddr},
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    sync::Arc,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, Result as IoResult},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
};
use tokio01::util::FutureExt;
use tokio_util::codec::{Encoder, FramedRead, FramedWrite, LinesCodec};

#[macro_export]
macro_rules! assert_downcast_matches {
    ($e:expr, $t:ty, $v:pat) => {{
        match $e.downcast_ref::<$t>() {
            Some($v) => (),
            got => panic!("assertion failed: got wrong error variant {:?}", got),
        }
    }};
}

static NEXT_PORT: AtomicUsize = AtomicUsize::new(1234);

pub fn next_addr() -> SocketAddr {
    use std::net::{IpAddr, Ipv4Addr};

    let port = NEXT_PORT.fetch_add(1, Ordering::AcqRel) as u16;
    SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), port)
}

pub fn trace_init() {
    let env = std::env::var("TEST_LOG").unwrap_or_else(|_| "off".to_string());

    let subscriber = tracing_subscriber::FmtSubscriber::builder()
        .with_env_filter(env)
        .finish();

    let _ = tracing_log::LogTracer::init();
    let _ = tracing::dispatcher::set_global_default(tracing::Dispatch::new(subscriber));
}

pub async fn send_lines(
    addr: SocketAddr,
    lines: impl IntoIterator<Item = String>,
) -> Result<(), Infallible> {
    send_encodable(addr, LinesCodec::new(), lines).await
}

pub async fn send_encodable<I, E: From<std::io::Error> + std::fmt::Debug>(
    addr: SocketAddr,
    encoder: impl Encoder<I, Error = E>,
    lines: impl IntoIterator<Item = I>,
) -> Result<(), Infallible> {
    let stream = TcpStream::connect(&addr).await.unwrap();
    let mut sink = FramedWrite::new(stream, encoder);

    let mut lines = stream::iter(lines.into_iter()).map(Ok);
    sink.send_all(&mut lines).await.unwrap();

    let stream = sink.get_mut();
    stream.shutdown(Shutdown::Both).unwrap();

    Ok(())
}

pub async fn send_lines_tls(
    addr: SocketAddr,
    host: String,
    lines: impl Iterator<Item = String>,
) -> Result<(), Infallible> {
    let stream = TcpStream::connect(&addr).await.unwrap();

    let mut connector = SslConnector::builder(SslMethod::tls()).unwrap();
    connector.set_verify(SslVerifyMode::NONE);
    let config = connector.build().configure().unwrap();

    let stream = tokio_openssl::connect(config, &host, stream).await.unwrap();
    let mut sink = FramedWrite::new(stream, LinesCodec::new());

    let mut lines = stream::iter(lines).map(Ok);
    sink.send_all(&mut lines).await.unwrap();

    let stream = sink.get_mut().get_mut();
    stream.shutdown(Shutdown::Both).unwrap();

    Ok(())
}

pub fn temp_file() -> PathBuf {
    let path = std::env::temp_dir();
    let file_name = random_string(16);
    path.join(file_name + ".log")
}

pub fn temp_dir() -> PathBuf {
    let path = std::env::temp_dir();
    let dir_name = random_string(16);
    path.join(dir_name)
}

pub fn random_lines_with_stream(
    len: usize,
    count: usize,
) -> (Vec<String>, impl Stream01<Item = Event, Error = ()>) {
    let lines = (0..count).map(|_| random_string(len)).collect::<Vec<_>>();
    let stream = stream01::iter_ok(lines.clone().into_iter().map(Event::from));
    (lines, stream)
}

pub fn random_events_with_stream(
    len: usize,
    count: usize,
) -> (Vec<Event>, impl Stream01<Item = Event, Error = ()>) {
    random_events_with_stream_generic(count, move || Event::from(random_string(len)))
}

pub fn random_nested_events_with_stream(
    len: usize,
    breadth: usize,
    depth: usize,
    count: usize,
) -> (Vec<Event>, impl Stream01<Item = Event, Error = ()>) {
    random_events_with_stream_generic(count, move || {
        let mut log = LogEvent::default();

        let tree = random_pseudonested_map(len, breadth, depth);
        for (k, v) in tree.into_iter() {
            log.insert(k, v);
        }

        Event::Log(log)
    })
}

pub fn random_string(len: usize) -> String {
    thread_rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .collect::<String>()
}

pub fn random_lines(len: usize) -> impl Iterator<Item = String> {
    std::iter::repeat(()).map(move |_| random_string(len))
}

pub fn random_map(max_size: usize, field_len: usize) -> HashMap<String, String> {
    let size = thread_rng().gen_range(0, max_size);

    (0..size)
        .map(move |_| (random_string(field_len), random_string(field_len)))
        .collect()
}

pub fn random_maps(
    max_size: usize,
    field_len: usize,
) -> impl Iterator<Item = HashMap<String, String>> {
    iter::repeat(()).map(move |_| random_map(max_size, field_len))
}

pub fn collect_n<T>(mut rx: mpsc::Receiver<T>, n: usize) -> impl Future<Item = Vec<T>, Error = ()> {
    let mut events = Vec::new();

    future::poll_fn(move || {
        while events.len() < n {
            let e = try_ready!(rx.poll()).unwrap();
            events.push(e);
        }
        Ok(Async::Ready(mem::replace(&mut events, Vec::new())))
    })
}

pub fn lines_from_file<P: AsRef<Path>>(path: P) -> Vec<String> {
    trace!(message = "Reading file.", path = %path.as_ref().display());
    let mut file = File::open(path).unwrap();
    let mut output = String::new();
    file.read_to_string(&mut output).unwrap();
    output.lines().map(|s| s.to_owned()).collect()
}

pub fn wait_for(mut f: impl FnMut() -> bool) {
    let wait = std::time::Duration::from_millis(5);
    let limit = std::time::Duration::from_secs(5);
    let mut attempts = 0;
    while !f() {
        std::thread::sleep(wait);
        attempts += 1;
        if attempts * wait > limit {
            panic!("timed out while waiting");
        }
    }
}

pub fn block_on<F, R, E>(future: F) -> Result<R, E>
where
    F: Send + 'static + Future<Item = R, Error = E>,
    R: Send + 'static,
    E: Send + 'static,
{
    let mut rt = runtime();

    rt.block_on(future)
}

pub fn block_on_std<F>(future: F) -> F::Output
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let mut rt = runtime();

    rt.block_on_std(future)
}

pub fn runtime() -> Runtime {
    Runtime::single_threaded().unwrap()
}

pub fn wait_for_tcp(addr: SocketAddr) {
    wait_for(|| std::net::TcpStream::connect(addr).is_ok())
}

pub fn wait_for_atomic_usize<T, F>(val: T, unblock: F)
where
    T: AsRef<AtomicUsize>,
    F: Fn(usize) -> bool,
{
    let val = val.as_ref();
    wait_for(|| unblock(val.load(Ordering::SeqCst)))
}

pub fn shutdown_on_idle(runtime: Runtime) {
    block_on(
        runtime
            .shutdown_on_idle()
            .timeout(std::time::Duration::from_secs(10)),
    )
    .unwrap()
}

#[derive(Debug)]
pub struct CollectN<S>
where
    S: Stream01,
{
    stream: Option<S>,
    remaining: usize,
    items: Option<Vec<S::Item>>,
}

impl<S: Stream01> CollectN<S> {
    pub fn new(s: S, n: usize) -> Self {
        Self {
            stream: Some(s),
            remaining: n,
            items: Some(Vec::new()),
        }
    }
}

impl<S> Future for CollectN<S>
where
    S: Stream01,
{
    type Item = (S, Vec<S::Item>);
    type Error = S::Error;

    fn poll(&mut self) -> Poll<(S, Vec<S::Item>), S::Error> {
        let stream = self.stream.take();
        if stream.is_none() {
            panic!("Stream is missing");
        }
        let mut stream = stream.unwrap();

        loop {
            if self.remaining == 0 {
                return Ok(Async::Ready((stream, self.items.take().unwrap())));
            }
            match stream.poll() {
                Ok(Async::Ready(Some(e))) => {
                    self.items.as_mut().unwrap().push(e);
                    self.remaining -= 1;
                }
                Ok(Async::Ready(None)) => {
                    return Ok(Async::Ready((stream, self.items.take().unwrap())));
                }
                Ok(Async::NotReady) => {
                    self.stream.replace(stream);
                    return Ok(Async::NotReady);
                }
                Err(e) => {
                    return Err(e);
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct CollectCurrent<S>
where
    S: Stream01,
{
    stream: Option<S>,
}

impl<S: Stream01> CollectCurrent<S> {
    pub fn new(s: S) -> Self {
        Self { stream: Some(s) }
    }
}

impl<S> Future for CollectCurrent<S>
where
    S: Stream01,
{
    type Item = (S, Vec<S::Item>);
    type Error = S::Error;

    fn poll(&mut self) -> Poll<(S, Vec<S::Item>), S::Error> {
        if let Some(mut stream) = self.stream.take() {
            let mut items = vec![];

            loop {
                match stream.poll() {
                    Ok(Async::Ready(Some(e))) => items.push(e),
                    Ok(Async::Ready(None)) | Ok(Async::NotReady) => {
                        return Ok(Async::Ready((stream, items)));
                    }
                    Err(e) => {
                        return Err(e);
                    }
                }
            }
        } else {
            panic!("Future already completed");
        }
    }
}

pub struct CountReceiver<T> {
    count: Arc<AtomicUsize>,
    trigger: oneshot::Sender<()>,
    handle: JoinHandle<Vec<T>>,
}

impl<T: Send + 'static> CountReceiver<T> {
    pub fn count(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }

    pub async fn wait(self) -> Vec<T> {
        let _ = self.trigger.send(());
        self.handle.await.unwrap()
    }

    fn new<F, Fut>(make_fut: F) -> CountReceiver<T>
    where
        F: FnOnce(Arc<AtomicUsize>, oneshot::Receiver<()>) -> Fut,
        Fut: std::future::Future<Output = Vec<T>> + Send + 'static,
    {
        let count = Arc::new(AtomicUsize::new(0));
        let (trigger, tripwire) = oneshot::channel();

        CountReceiver {
            count: Arc::clone(&count),
            trigger,
            handle: tokio::spawn(make_fut(count, tripwire)),
        }
    }
}

impl CountReceiver<String> {
    pub fn receive_lines(addr: SocketAddr) -> CountReceiver<String> {
        CountReceiver::new(|count, tripwire| async move {
            let mut listener = TcpListener::bind(addr).await.unwrap();
            CountReceiver::receive_lines_stream(listener.incoming(), count, tripwire).await
        })
    }

    #[cfg(all(feature = "tokio/uds", unix))]
    pub fn receive_lines_unix<P>(path: P) -> CountReceiver<String>
    where
        P: AsRef<Path> + Send + 'static,
    {
        CountReceiver::new(|count, tripwire| async move {
            let mut listener = tokio::net::UnixListener::bind(path).unwrap();
            CountReceiver::receive_lines_stream(listener.incoming(), count, tripwire).await
        })
    }

    async fn receive_lines_stream<S, T>(
        stream: S,
        count: Arc<AtomicUsize>,
        tripwire: oneshot::Receiver<()>,
    ) -> Vec<String>
    where
        S: Stream<Item = IoResult<T>>,
        T: AsyncWrite + AsyncRead,
    {
        stream
            .take_until(tripwire)
            .map_ok(|socket| FramedRead::new(socket, LinesCodec::new()))
            .map(|x| x.unwrap())
            .flatten()
            .map(|x| x.unwrap())
            .inspect(move |_| {
                count.fetch_add(1, Ordering::Relaxed);
            })
            .collect::<Vec<String>>()
            .await
    }
}

impl CountReceiver<Event> {
    pub fn receive_events<S>(stream: S) -> CountReceiver<Event>
    where
        S: Stream01<Item = Event> + Send + 'static,
        <S as Stream01>::Error: std::fmt::Debug,
    {
        CountReceiver::new(|count, tripwire| async move {
            stream
                .compat()
                .take_until(tripwire)
                .map(|x| x.unwrap())
                .inspect(move |_| {
                    count.fetch_add(1, Ordering::Relaxed);
                })
                .collect::<Vec<Event>>()
                .await
        })
    }
}

fn random_events_with_stream_generic<F>(
    count: usize,
    generator: F,
) -> (Vec<Event>, impl Stream01<Item = Event, Error = ()>)
where
    F: Fn() -> Event,
{
    let events = (0..count).map(|_| generator()).collect::<Vec<_>>();
    let stream = stream01::iter_ok(events.clone().into_iter());
    (events, stream)
}

fn random_pseudonested_map(len: usize, breadth: usize, depth: usize) -> HashMap<String, String> {
    if breadth == 0 || depth == 0 {
        return HashMap::new();
    }

    if depth == 1 {
        let mut leaf = HashMap::new();
        leaf.insert(random_string(len), random_string(len));
        return leaf;
    }

    let mut tree = HashMap::new();
    for _ in 0..breadth {
        let prefix = random_string(len);
        let subtree = random_pseudonested_map(len, breadth, depth - 1);

        let subtree: HashMap<String, String> = subtree
            .into_iter()
            .map(|(mut key, value)| {
                key.insert(0, '.');
                key.insert_str(0, &prefix[..]);
                (key, value)
            })
            .collect();

        for (key, value) in subtree.into_iter() {
            tree.insert(key, value);
        }
    }
    tree
}
