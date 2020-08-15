use crate::AsyncWrite;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

/// Helper used in both cliend and server
#[instrument(skip(cx, io, to_write, to_write_flush_after))]
pub(crate) fn try_write<S: AsyncWrite + Unpin>(
    cx: &mut Context<'_>,
    io: &mut S,
    to_write: &mut Vec<u8>,
    to_write_flush_after: &mut bool,
) -> io::Result<bool> {
    if to_write.is_empty() {
        if *to_write_flush_after {
            trace!("try_write attempt flush");

            match Pin::new(io).poll_flush(cx) {
                Poll::Pending => {
                    return Ok(false);
                }
                Poll::Ready(Ok(_)) => {
                    trace!("try_write flushed");
                    // flush done
                    *to_write_flush_after = false;
                }
                Poll::Ready(Err(e)) => {
                    trace!("try_write error: {:?}", e);
                    return Err(e).into();
                }
            }
        }

        return Ok(false);
    }

    trace!("try_write left: {}", to_write.len());

    let poll = Pin::new(io).poll_write(cx, &to_write);

    match poll {
        Poll::Pending => {
            // Pending is fine. It means the socket is full upstream, we can still
            // progress the downstream (i.e. drive_state()).
            trace!("try_write: Poll::Pending");
            return Ok(false);
        }

        // We managed to write some.
        Poll::Ready(Ok(amount)) => {
            trace!("try_write did write: {}", amount);
            // TODO: some more efficient buffer?
            let remain = to_write.split_off(amount);
            *to_write = remain;
        }

        Poll::Ready(Err(e)) => {
            trace!("try_write error: {:?}", e);
            return Err(e).into();
        }
    }

    Ok(true)
}
