pub fn make_io_error(desc: &str) -> std::io::Error {
    std::io::Error::other(desc)
}
