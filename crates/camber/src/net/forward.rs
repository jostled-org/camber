use crate::RuntimeError;
use tokio::io::{AsyncRead, AsyncWrite};

/// Copy bytes bidirectionally between two async IO streams until either side closes.
///
/// Returns `(a_to_b, b_to_a)` — the number of bytes copied in each direction.
pub async fn forward<A, B>(mut a: A, mut b: B) -> Result<(u64, u64), RuntimeError>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let counts = tokio::io::copy_bidirectional(&mut a, &mut b).await?;
    Ok(counts)
}
