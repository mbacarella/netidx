use crate::{
    channel::Channel,
    path::Path,
    resolver_store::Store,
};
use futures::{
    channel::oneshot,
    future::{FutureExt as FRSFutureExt},
};
use std::{
    result, mem, io,
    sync::{Arc, atomic::{AtomicUsize, Ordering}},
    time::Duration,
    net::SocketAddr,
};
use async_std::{
    prelude::*,
    task,
    future,
    net::{TcpStream, TcpListener},
};
use serde::Serialize;
use failure::Error;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ClientHello {
    ReadOnly,
    WriteOnly { ttl: u64, write_addr: SocketAddr }
}
 

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ServerHello { pub ttl_expired: bool }

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum ToResolver {
    Resolve(Vec<Path>),
    List(Path),
    Publish(Vec<Path>),
    Unpublish(Vec<Path>),
    Clear
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum FromResolver {
    Resolved(Vec<Vec<SocketAddr>>),
    List(Vec<Path>),
    Published,
    Unpublished,
    Error(String)
}

type ClientInfo = Option<oneshot::Sender<()>>;

fn handle_batch(
    store: &Store<ClientInfo>,
    msgs: impl Iterator<Item = ToResolver>,
    con: &mut Channel,
    wa: Option<SocketAddr>
) -> Result<(), Error> {
    match wa {
        None => {
            let s = store.read();
            for m in msgs {
                match m {
                    ToResolver::Resolve(paths) => {
                        let res = paths.iter().map(|p| s.resolve(p)).collect();
                        con.queue_send(&FromResolver::Resolved(res))?
                    },
                    ToResolver::List(path) => {
                        con.queue_send(&FromResolver::List(s.list(&path)))?
                    }
                    ToResolver::Publish(_)
                        | ToResolver::Unpublish(_)
                        | ToResolver::Clear =>
                        con.queue_send(&FromResolver::Error("read only".into()))?,
                }
            }
        }
        Some(write_addr) => {
            let mut s = store.write();
            for m in msgs {
                match m {
                    ToResolver::Resolve(_) | ToResolver::List(_) =>
                        con.queue_send(&FromResolver::Error("write only".into()))?,
                    ToResolver::Publish(paths) => {
                        if !paths.iter().all(Path::is_absolute) {
                            con.queue_send(
                                &FromResolver::Error("absolute paths required".into())
                            )?
                        } else {
                            for path in paths {
                                s.publish(path, write_addr);
                            }
                            con.queue_send(&FromResolver::Published)?
                        }
                    }
                    ToResolver::Unpublish(paths) => {
                        for path in paths {
                            s.unpublish(path, write_addr);
                        }
                        con.queue_send(&FromResolver::Unpublished)?
                    }
                    ToResolver::Clear => {
                        s.unpublish_addr(write_addr);
                        s.gc();
                        con.queue_send(&FromResolver::Unpublished)?
                    }
                }
            }
        }
    }
    Ok(())
}

async fn client_loop(
    store: Store<ClientInfo>,
    s: TcpStream,
    server_stop: impl Future<Output = result::Result<(), oneshot::Canceled>>,
) -> Result<(), Error> {
    #[derive(Debug)]
    enum M {
        Stop,
        Timeout,
        Msg(result::Result<(), io::Error>)
    }
    s.set_nodelay(true)?;
    let mut con = Channel::new(s);
    let hello: ClientHello = con.receive().await?;
    let (tx_stop, rx_stop) = oneshot::channel();
    let (ttl, ttl_expired, write_addr) = match hello {
        ClientHello::ReadOnly => (Duration::from_secs(120), false, None),
        ClientHello::WriteOnly {ttl, write_addr} => {
            if ttl <= 0 || ttl > 3600 { bail!("invalid ttl") }
            let mut store = store.write();
            let clinfos = store.clinfo_mut();
            let ttl = Duration::from_secs(ttl);
            match clinfos.get_mut(&write_addr) {
                None => {
                    clinfos.insert(write_addr, Some(tx_stop));
                    (ttl, true, Some(write_addr))
                },
                Some(cl) => {
                    if let Some(old_stop) = mem::replace(cl, Some(tx_stop)) {
                        let _ = old_stop.send(());
                    }
                    (ttl, false, Some(write_addr))
                }
            }
        }
    };
    con.send_one(&ServerHello { ttl_expired }).await?;
    let mut con = Some(con);
    let server_stop = server_stop.shared();
    let rx_stop = rx_stop.shared();
    let mut batch = Vec::new();
    loop {
        let msg = match con {
            None => future::pending::<M>().left_future(),
            Some(ref mut con) =>
                con.receive_batch(&mut batch).map(|r| M::Msg(r)).right_future()
        };
        let timeout = future::ready(M::Timeout).delay(ttl);
        let stop =
            server_stop.clone().map(|_| M::Stop)
            .race(rx_stop.clone().map(|_| M::Stop));
        match dbg!(msg.race(stop).race(timeout).await) {
            M::Stop => break Ok(()),
            M::Msg(Err(e)) => {
                batch.clear();
                con = None;
                // CR estokes: use proper log module
                println!("error reading message: {}", e)
            },
            M::Msg(Ok(())) => match con {
                None => { batch.clear(); }
                Some(ref mut c) => {
                    match handle_batch(&store, batch.drain(..), c, write_addr) {
                        Err(_) => { con = None },
                        Ok(()) => match c.flush().await {
                            Err(_) => { con = None }, // CR estokes: Log this
                            Ok(()) => ()
                        }
                    }
                }
            }
            M::Timeout => {
                if let Some(write_addr) = write_addr {
                    let mut store = store.write();
                    if let Some(ref mut cl) = store.clinfo_mut().remove(&write_addr) {
                        if let Some(stop) = mem::replace(cl, None) {
                            let _ = stop.send(());
                        }
                    }
                    store.unpublish_addr(write_addr);
                    store.gc();
                }
                bail!("client timed out");
            }
        }
    }
}

async fn server_loop(
    addr: SocketAddr,
    max_connections: usize,
    stop: oneshot::Receiver<()>,
    ready: oneshot::Sender<SocketAddr>,
) -> Result<SocketAddr, Error> {
    enum M { Stop, Drop, Client(TcpStream) }
    let connections = Arc::new(AtomicUsize::new(0));
    let published: Store<ClientInfo> = Store::new();
    let listener = TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;
    let stop = stop.shared();
    let _ = ready.send(local_addr);
    loop {
        let client = listener.accept().map(|c| match c {
            Ok((c, _)) => M::Client(c),
            Err(_) => M::Drop, // CR estokes: maybe log this?
        });
        let should_stop = stop.clone().map(|_| M::Stop);
        match should_stop.race(client).await {
            M::Stop => return Ok(local_addr),
            M::Drop => (),
            M::Client(client) => {
                if connections.fetch_add(1, Ordering::Relaxed) < max_connections {
                    let connections = connections.clone();
                    let published = published.clone();
                    let stop = stop.clone();
                    task::spawn(async move {
                        let _ = client_loop(published, client, stop).await;
                        connections.fetch_sub(1, Ordering::Relaxed);
                    });
                }
            },
        }
    }
}

#[derive(Debug)]
pub struct Server {
    stop: Option<oneshot::Sender<()>>,
    local_addr: SocketAddr,
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(stop) = mem::replace(&mut self.stop, None) {
            let _ = stop.send(());
        }
    }
}

impl Server {
    pub async fn new(addr: SocketAddr, max_connections: usize) -> Result<Server, Error> {
        let (send_stop, recv_stop) = oneshot::channel();
        let (send_ready, recv_ready) = oneshot::channel();
        let local_addr =
            task::spawn(server_loop(addr, max_connections, recv_stop, send_ready))
            .race(recv_ready.map(|r| r.map_err(|e| Error::from(e))))
            .await?;
        Ok(Server {
            stop: Some(send_stop),
            local_addr
        })
    }

    pub fn local_addr(&self) -> &SocketAddr {
        &self.local_addr
    }
}

#[cfg(test)]
mod test {
    use std::net::SocketAddr;
    use crate::{
        path::Path,
        resolver_server::Server,
        resolver::{WriteOnly, ReadOnly, Resolver},
    };

    async fn init_server() -> Server {
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        Server::new(addr, 100).await.expect("start server")
    }

    fn p(p: &str) -> Path {
        Path::from(p)
    }

    #[test]
    fn publish_resolve() {
        use async_std::task;
        task::block_on(async {
            let server = init_server().await;
            let paddr: SocketAddr = "127.0.0.1:1".parse().unwrap();
            let mut w = Resolver::<WriteOnly>::new_w(server.local_addr(), paddr).unwrap();
            let mut r = Resolver::<ReadOnly>::new_r(server.local_addr()).unwrap();
            let paths = vec![
                p("/foo/bar"),
                p("/foo/baz"),
                p("/app/v0"),
                p("/app/v1"),
            ];
            w.publish(paths.clone()).await.unwrap();
            for addrs in r.resolve(paths.clone()).await.unwrap() {
                assert_eq!(addrs.len(), 1);
                assert_eq!(addrs[0], paddr);
            }
            assert_eq!(
                r.list(p("/")).await.unwrap(),
                vec![p("/app"), p("/foo")]
            );
            assert_eq!(
                r.list(p("/foo")).await.unwrap(),
                vec![p("/foo/bar"), p("/foo/baz")]
            );
            assert_eq!(
                r.list(p("/app")).await.unwrap(),
                vec![p("/app/v0"), p("/app/v1")]
            );
        });
    }
}
