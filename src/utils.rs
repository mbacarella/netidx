use bytes::{BytesMut, BigEndian, ByteOrder};
use futures_codec::{Encoder, Decoder};
use std::{
    mem,
    convert::TryInto,
    marker::PhantomData,
    io::{self, Write},
    result::Result,
};
use serde::{Serialize, de::DeserializeOwned};
use failure::Error;

struct BytesWriter<'a>(&'a mut BytesMut);

impl Write for BytesWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
	self.0.extend(buf);
	Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
	Ok(())
    }
}

static U64S: usize = mem::size_of::<u64>();

pub struct MPCodec<T, F>(PhantomData<T>, PhantomData<F>);

impl<T, F> MPCodec<T, F> {
    pub fn new() -> MPCodec<T, F> {
        MPCodec(PhantomData, PhantomData)
    }
}

impl<T: Serialize, F: DeserializeOwned> Encoder for MPCodec<T, F> {
    type Item = T;
    type Error = Error;

    fn encode(&mut self, src: Self::Item, dst: &mut BytesMut) -> Result<(), Self::Error> {
        if dst.capacity() < U64S {
            dst.reserve(U64S * 20);
        }
        let mut header = dst.split_to(U64S);
        let res = rmp_serde::encode::write_named(&mut BytesWriter(dst), &src);
        BigEndian::write_u64(&mut header, dst.len() as u64);
        Ok(res?)
    }
}

impl<T: Serialize, F: DeserializeOwned> Decoder for MPCodec<T, F> {
    type Item = F;
    type Error = Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.len() < U64S {
            Ok(None)
        } else {
            let len: usize =
                BigEndian::read_u64(src).try_into()?;
            if src.len() - U64S < len {
                Ok(None)
            } else {
                src.advance(U64S);
                let res = rmp_serde::decode::from_read(src.as_ref());
                src.advance(len);
                Ok(Some(res?))
            }
        }
    }
}

use async_std::prelude::*;
use futures::task::{Poll, Context};
use std::pin::Pin;

pub(crate) fn batched<S: Stream>(stream: S, max: usize) -> Batched<S> {
    Batched {
        stream, max,
        ended: false,
        current: 0
    }
}

pub enum BatchItem<T> {
    InBatch(T),
    EndBatch
}

#[must_use = "streams do nothing unless polled"]
pub(crate) struct Batched<S: Stream> {
    stream: S,
    ended: bool,
    max: usize,
    current: usize,
}

impl<S: Stream> Stream for Batched<S> {
    type Item = BatchItem<<S as Stream>::Item>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        if self.ended {
            Poll::Ready(None)
        } else if self.current >= self.max {
            self.current = 0;
            Poll::Ready(Some(BatchItem::EndBatch))
        } else {
            match self.stream.poll_next(cx) {
                Poll::Ready(Some(v)) => {
                    self.current += 1;
                    Poll::Ready(Some(BatchItem::InBatch(v)))
                },
                Poll::Ready(None) => {
                    self.ended = true;
                    if self.current == 0 {
                        Poll::Ready(None)
                    } else {
                        Poll::Ready(Some(BatchItem::EndBatch))
                    }
                },
                Poll::Pending => {
                    if self.current == 0 {
                        Poll::Pending
                    } else {
                        Poll::Ready(Some(BatchItem::EndBatch))
                    }
                }
            }
        }
    }
}
