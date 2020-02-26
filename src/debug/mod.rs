use super::rmux::dump_session_state;

pub async fn handle_debug_server(debug_server: tiny_http::Server) {
    for request in debug_server.incoming_requests() {
        // println!(
        //     "received request! method: {:?}, url: {:?}, headers: {:?}",
        //     request.method(),
        //     request.url(),
        //     request.headers()
        // );

        if request.url() == "/stat" {
            let s = tiny_http::Response::from_string(dump_session_state());
            let _ = request.respond(s);
        } else {
            let response = tiny_http::Response::from_string("hello world");
            let _ = request.respond(response);
        }
    }
}
