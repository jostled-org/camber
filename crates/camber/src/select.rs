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
/// Task cancellation is delivered as `Err(Cancelled)` to the first receive
/// arm, matching the cancellation behavior of `Receiver::recv`.
/// All arms must produce the same type.
#[macro_export]
macro_rules! select {
    // Internal: accumulate recv arms while retaining the first arm's typed
    // cancellation result. Cancellation follows the same result path a closed
    // first receiver would, so the macro keeps its existing output type.
    (@build [$($arms:tt)*] [$($cancelled:tt)*] $val:ident = $rx:expr => $body:expr, $($rest:tt)*) => {
        $crate::select!(@build [
            $($arms)*
            recv($rx.as_crossbeam()) -> __msg => {
                let $val = match $crate::channel::__task_cancelled() {
                    true => $rx.__cancelled_receive(),
                    false => __msg.map_err(|_| $crate::RuntimeError::ChannelClosed),
                };
                $body
            },
        ] [$($cancelled)*] $($rest)*)
    };
    // Terminal: timeout arm last
    (@build [$($arms:tt)*] [$($cancelled:tt)*] timeout($dur:expr) => $body:expr $(,)?) => {{
        let __duration = $dur;
        let __started = std::time::Instant::now();
        match (
            $crate::channel::__task_cancelled(),
            $crate::channel::__task_cancel_receiver(),
        ) {
            (true, _) => { $($cancelled)* },
            (false, Some(__cancel)) => {
                $crate::__private::crossbeam_channel::select! {
                    $($arms)*
                    recv(__cancel) -> __cancelled_signal => match __cancelled_signal {
                        Ok(()) => { $($cancelled)* },
                        Err(_) => $crate::__private::crossbeam_channel::select! {
                            $($arms)*
                            default(__duration.saturating_sub(__started.elapsed())) => { $body }
                        },
                    },
                    default(__duration) => { $body }
                }
            }
            (false, None) => $crate::__private::crossbeam_channel::select! {
                $($arms)*
                default(__duration) => { $body }
            },
        }
    }};
    // Terminal: no timeout, no more arms
    (@build [$($arms:tt)*] [$($cancelled:tt)*]) => {{
        match (
            $crate::channel::__task_cancelled(),
            $crate::channel::__task_cancel_receiver(),
        ) {
            (true, _) => { $($cancelled)* },
            (false, Some(__cancel)) => {
                $crate::__private::crossbeam_channel::select! {
                    $($arms)*
                    recv(__cancel) -> __cancelled_signal => match __cancelled_signal {
                        Ok(()) => { $($cancelled)* },
                        Err(_) => $crate::__private::crossbeam_channel::select! {
                            $($arms)*
                        },
                    },
                }
            }
            (false, None) => $crate::__private::crossbeam_channel::select! {
                $($arms)*
            },
        }
    }};

    // Entry: name the first receiver once so the cancellation arm can create
    // a correctly typed `Err(Cancelled)` without re-evaluating its expression.
    ($val:ident = $rx:expr => $body:expr, $($rest:tt)*) => {{
        let __first_receiver = &$rx;
        $crate::select!(@build [
            recv(__first_receiver.as_crossbeam()) -> __msg => {
                let $val = match $crate::channel::__task_cancelled() {
                    true => __first_receiver.__cancelled_receive(),
                    false => __msg.map_err(|_| $crate::RuntimeError::ChannelClosed),
                };
                $body
            },
        ] [{
            let $val = __first_receiver.__cancelled_receive();
            $body
        }] $($rest)*)
    }};
}
