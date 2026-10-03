//! Counter/gauge helpers for mux flow-control and control-channel diagnostics.
//! All per-connection series use a `conn_id` label (TLS pool index).

fn conn(conn_id: u32) -> String {
    conn_id.to_string()
}

#[allow(dead_code)]
pub(crate) fn gauge_streams_inc(conn_id: u32) {
    metrics::gauge!("mux.streams", "conn_id" => conn(conn_id)).increment(1.0);
}

#[allow(dead_code)]
pub(crate) fn gauge_streams_dec(conn_id: u32, count: f64) {
    if count > 0.0 {
        metrics::gauge!("mux.streams", "conn_id" => conn(conn_id)).decrement(count);
    }
}

#[allow(dead_code)]
pub(crate) fn set_flow_gauges(
    conn_id: u32,
    total_recv_window: u64,
    total_send_window: u64,
    total_pending_bytes: u64,
) {
    let c = conn(conn_id);
    metrics::gauge!("mux.stream.total_recv_window", "conn_id" => c.clone())
        .set(total_recv_window as f64);
    metrics::gauge!("mux.stream.total_send_window", "conn_id" => c.clone())
        .set(total_send_window as f64);
    metrics::gauge!("mux.stream.total_pending_bytes", "conn_id" => c)
        .set(total_pending_bytes as f64);
}

#[allow(dead_code)]
pub(crate) fn inc_flow_violation(conn_id: u32) {
    metrics::counter!("mux.stream.flow_violation", "conn_id" => conn(conn_id)).increment(1);
}

#[allow(dead_code)]
pub(crate) fn inc_inbound_receiver_dropped(conn_id: u32) {
    metrics::counter!("mux.stream.inbound_receiver_dropped", "conn_id" => conn(conn_id))
        .increment(1);
}

#[allow(dead_code)]
pub(crate) fn inc_read_enqueue_failed(conn_id: u32) {
    metrics::counter!("mux.control.read_enqueue_failed", "conn_id" => conn(conn_id)).increment(1);
}

#[allow(dead_code)]
pub(crate) fn inc_window_update_wire_failed(conn_id: u32) {
    metrics::counter!("mux.window_update.wire_write_failed", "conn_id" => conn(conn_id))
        .increment(1);
}

pub(crate) fn inc_stream_close_drop_failed(conn_id: u32) {
    metrics::counter!("mux.stream.close_drop_failed", "conn_id" => conn(conn_id)).increment(1);
}

pub(crate) fn inc_write_window_wait(conn_id: u32) {
    metrics::counter!("mux.stream.write_window_wait", "conn_id" => conn(conn_id)).increment(1);
}

pub(crate) fn inc_poll_reserve_wait(conn_id: u32) {
    metrics::counter!("mux.control.poll_reserve_wait", "conn_id" => conn(conn_id)).increment(1);
}

pub(crate) fn inc_poll_reserve_aborted(conn_id: u32) {
    metrics::counter!("mux.poll_reserve.aborted", "conn_id" => conn(conn_id)).increment(1);
}

#[allow(dead_code)]
pub(crate) fn inc_open_stream_enqueue_failed(conn_id: u32) {
    metrics::counter!("mux.open_stream.enqueue_failed", "conn_id" => conn(conn_id)).increment(1);
}

#[allow(dead_code)]
pub(crate) fn inc_ping_timeout(conn_id: u32) {
    metrics::counter!("mux.ping.timeout", "conn_id" => conn(conn_id)).increment(1);
}

#[allow(dead_code)]
pub(crate) fn inc_ping_failed(conn_id: u32) {
    metrics::counter!("mux.ping.failed", "conn_id" => conn(conn_id)).increment(1);
}

#[allow(dead_code)]
pub(crate) fn inc_ping_enqueue_failed(conn_id: u32) {
    metrics::counter!("mux.ping.enqueue_failed", "conn_id" => conn(conn_id)).increment(1);
}

#[allow(dead_code)]
pub(crate) fn inc_client_open_stream_failed() {
    metrics::counter!("client.open_stream.failed").increment(1);
}
