pub fn make_io_error(desc: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, desc)
}
