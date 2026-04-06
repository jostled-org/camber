/// Select over multiple channel operations with optional timeout.
///
/// # Syntax
///
/// ```ignore
/// camber::select! {
///     val = rx1 => expr1,
///     val = rx2 => expr2,
///     timeout(duration) => expr3,
/// }
/// ```
///
/// Each recv arm binds the received `Result<T, RuntimeError>` to `val`.
/// The timeout arm fires if no channel is ready within the given `Duration`.
/// All arms must produce the same type.
#[macro_export]
macro_rules! select {
    // Internal: accumulate recv arms, then emit crossbeam select!
    (@build [$($arms:tt)*] $val:ident = $rx:expr => $body:expr, $($rest:tt)*) => {
        $crate::select!(@build [
            $($arms)*
            recv($rx.as_crossbeam()) -> __msg => {
                let $val = __msg.map_err(|_| $crate::RuntimeError::ChannelClosed);
                $body
            },
        ] $($rest)*)
    };
    // Terminal: timeout arm last
    (@build [$($arms:tt)*] timeout($dur:expr) => $body:expr $(,)?) => {
        $crate::__private::crossbeam_channel::select! {
            $($arms)*
            default($dur) => { $body }
        }
    };
    // Terminal: no timeout, no more arms
    (@build [$($arms:tt)*]) => {
        $crate::__private::crossbeam_channel::select! {
            $($arms)*
        }
    };

    // Entry: starts accumulation
    ($($tokens:tt)*) => {
        $crate::select!(@build [] $($tokens)*)
    };
}
