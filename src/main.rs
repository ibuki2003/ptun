use std::env;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
// use tokio::io;
use tokio::net::UdpSocket;
// use tokio::prelude::*;
use bytes::{BytesMut, Buf};
use std::sync::Arc;


struct UdpUnpacker {
  buf: BytesMut,
  current_id: Option<u32>,
}

impl UdpUnpacker {
  fn new() -> Self {
    UdpUnpacker {
      buf: BytesMut::new(),
      current_id: None,
    }
  }

  async fn push_raw(&mut self, buf: &[u8]) {
    if buf.len() < 4 {
      panic!();
    }
    let new_id = u32::from_le_bytes(buf[0..4].try_into().unwrap());

    match self.current_id {
      Some(id) => {
        if id != new_id {
          self.buf.clear();
          self.current_id = Some(new_id);
        }
      }
      None => {
        self.current_id = Some(new_id);
      }
    }

    self.buf.extend_from_slice(&buf[4..]);
  }

  async fn get(&mut self) -> Option<bytes::Bytes> {
    if self.buf.len() < 4 { return None; }

    let mut cur = std::io::Cursor::new(&self.buf[..]);
    let len = cur.read_u16().await.unwrap() as usize;
    if len > self.buf.len() - 2 {
      return None;
    }

    self.buf.advance(2);
    let data = self.buf.split_to(len);
    self.buf.clear();
    Some(data.freeze())
  }
}

struct Pack {
  id: u32,
  data: bytes::Bytes,

  first: bool,
}

impl std::iter::Iterator for Pack {
  type Item = bytes::Bytes;

  fn next(self: &mut Self) -> Option<Self::Item> {
    if !self.data.has_remaining() {
      return None
    }

    let mut data = bytes::BytesMut::new();
    data.extend_from_slice(&self.id.to_be_bytes());
    if self.first {
      data.extend_from_slice(&(self.data.remaining() as u16).to_be_bytes());
      self.first = false;
    }
    let len = std::cmp::min(self.data.remaining(), 32768);
    data.extend_from_slice(&self.data.split_to(len).as_ref());
    Some(data.freeze())
  }
}

struct UdpPacker {
  id: u32,
}

impl UdpPacker {
  fn new() -> Self {
    UdpPacker {
      id: 0,
    }
  }

  fn pack(&mut self, buf: &bytes::Bytes) -> Pack {
    let pack = Pack {
      id: self.id,
      data: buf.clone(),
      first: true,
    };
    self.id += 1;
    pack
  }
}

fn udp_conn_send(socket: Arc<UdpSocket>) -> (mpsc::Sender<bytes::Bytes>, impl std::future::Future<Output=()>) {
  let (tx, mut rx) = mpsc::channel::<bytes::Bytes>(32);

  (tx, async move {
    let mut packer = UdpPacker::new();
    while let Some(buf) = rx.recv().await {
      for packet in packer.pack(&buf) {
        match socket.send(&packet).await {
          Ok(_) => {
          },
          Err(e) => {
            eprintln!("send error: {}", e);
            continue;
          }
        }
      }
    }
  })
}

fn udp_conn_recv(socket: Arc<UdpSocket>) -> (mpsc::Receiver<bytes::Bytes>, impl std::future::Future<Output=()>) {
  let (tx, rx) = mpsc::channel::<bytes::Bytes>(32);

  (rx, async move {
    let mut unpacker = UdpUnpacker::new();

    let mut buf = [0u8; 65536];
    loop {
      let n = socket.recv(&mut buf).await.unwrap();
      unpacker.push_raw(&buf[..n]).await;

      while let Some(data) = unpacker.get().await {
        let tx = tx.clone();
        tokio::spawn(async move {
        tx.send(data).await.unwrap();
        });
      }

    }
  })
}

#[tokio::main]
async fn main() -> () {
  let args: Vec<String> = env::args().collect();

  let iface = tokio_tun::TunBuilder::new()
    .name(&args[1])
    .tap(true)
    .try_build().unwrap();
  let (mut reader, mut writer) = tokio::io::split(iface);

  // TODO: make count configurable
  let conn1 = Arc::new(UdpSocket::bind(&args[2]).await.unwrap());
  conn1.connect(&args[3]).await.unwrap();
  let conn2 = Arc::new(UdpSocket::bind(&args[4]).await.unwrap());
  conn2.connect(&args[5]).await.unwrap();

  let conn1_1 = conn1.clone();
  let conn2_1 = conn2.clone();
  let recv = tokio::spawn(async move {
    let (mut conn1, job1) = udp_conn_recv(conn1_1);
    let (mut conn2, job2) = udp_conn_recv(conn2_1);
    tokio::spawn(job1);
    tokio::spawn(job2);

    loop {
      let buf = tokio::select! {
        Some(buf) = conn1.recv() => buf,
        Some(buf) = conn2.recv() => buf,
      };
      writer.write(&buf).await.unwrap();
    }
  });

  let conn1_1 = conn1.clone();
  let conn2_1 = conn2.clone();
  let send = tokio::spawn(async move {
    let (conn1, job1) = udp_conn_send(conn1_1);
    let (conn2, job2) = udp_conn_send(conn2_1);

    tokio::spawn(job1);
    tokio::spawn(job2);

    let mut buf = [0u8; 65536];
    loop {
      let n = reader.read(&mut buf).await.unwrap();
      if n == 0 {
        return;
      }
      let mut b = bytes::BytesMut::new();
      b.extend_from_slice(&buf[..n]);

      let buf = b.freeze();
      tokio::select! {
        _ = conn1.send(buf.clone()) => {}
        _ = conn2.send(buf.clone()) => {}
      }
    }
  });

  tokio::join!(recv, send);

}
