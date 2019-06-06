use url::Url;

pub fn get_hostport_from_url(url: &Url) -> Option<String> {
    let mut hostport = String::new();
    if let Some(host) = url.host_str() {
        hostport.push_str(host);
    } else {
        // Destructure failed. Change to the failure case.
        error!("no host found in url:{}", url);
        return None;
    };
    hostport.push_str(":");
    if let Some(port) = url.port() {
        hostport.push_str(port.to_string().as_str());
    } else {
        match url.scheme() {
            "http" => hostport.push_str("80"),
            "https" => hostport.push_str("443"),
            "tcp" => hostport.push_str("48100"),
            "tls" => hostport.push_str("443"),
            _ => {
                hostport.push_str("48100");
                warn!("use default '48100' port for scheme:{}", url.scheme());
            }
        }
    };
    return Some(hostport);
}
