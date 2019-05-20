extern crate byteorder;

use std::convert::From;
use std::default::Default;
use std::io::{self, Cursor};

use byteorder::{BigEndian, ReadBytesExt};

const CONTENT_TYPE_HANDSHAKE: u8 = 0x16;

type NetworkByteOrder = BigEndian;

pub type Opaque = u8;

#[derive(Debug)]
pub struct Random {
    gmt_unix_time: u32,
    random_bytes: [Opaque; 28]
}

impl Default for Random {
    fn default() -> Random {
        Random {
            gmt_unix_time: 0,
            random_bytes: [0; 28]
        }
    }
}

#[derive(Debug)]
pub struct CompressionMethod(pub u8);

#[derive(Debug)]
pub struct CipherSuite(pub u8, pub u8);

#[derive(Debug)]
pub struct ExtensionType(pub u16);

#[derive(Debug)]
pub struct Extension {
    pub extension_type: ExtensionType,
    pub length: u16,
    pub extension_data: Vec<Opaque>
}

/// ClientHello is the result of parsing the initial message from a TLS client
#[derive(Default, Debug)]
pub struct ClientHello {
    pub client_version: Version,
    pub random: Random,
    pub session_id: Vec<Opaque>,
    pub cipher_suites: Vec<CipherSuite>,
    pub compression_methods: Vec<CompressionMethod>,
    pub extensions: Vec<Extension>
}

#[derive(Default, Debug)]
pub struct Version {
    major: u8,
    minor: u8
}

#[derive(Debug)]
pub enum ContentType {
    Unknown,
    Handshake,
    Other
}

impl Default for ContentType {
    fn default() -> ContentType {
        ContentType::Unknown
    }
}

impl From<u8> for ContentType {
    fn from(u: u8) -> ContentType {
        match u {
            CONTENT_TYPE_HANDSHAKE => ContentType::Handshake,
            _ => ContentType::Other
        }
    }
}

/// ClientHelloBuilder produces a ClientHello struct from a stream of bytes
#[derive(Default, Debug)]
pub struct ClientHelloBuilder {
    /// client_hello is the object being built
    client_hello: ClientHello,

    content_type: ContentType,
    protocol_version: Version,
    length: u16,

    handshake_type: u8,
    handshake_length: u32, // handshake_length is actually 3 bytes

    session_id_length: u8,
    cipher_suites_length: u16,
    compression_methods_length: u8,
    extensions_length: u16,
}

impl ClientHelloBuilder {
    pub fn new() -> ClientHelloBuilder {
        Default::default()
    }

    /// Attempt to construct a ClientHello message using the provided bytes.
    ///
    /// The message must be provided in its entirety at this time. The parser does not support
    /// chunked messages.
    ///
    /// TODO: Fix horribly inefficient usage of read_u8
    pub fn parse_bytes(mut self, bytes: &[u8]) -> io::Result<ClientHello> {

        let mut state = State::Initial;
        let mut reader = Cursor::new(bytes);

        loop {
            match state {
                State::Initial => {
                    match ContentType::from(try!(reader.read_u8())) {
                        ContentType::Handshake => {
                            self.content_type = ContentType::Handshake;
                            state = State::ParseProtocolVersion;
                        },
                        _ => state = State::Fail("Bad content type"),
                    }
                },
                State::ParseProtocolVersion => {
                    self.protocol_version.major = try!(reader.read_u8());
                    self.protocol_version.minor = try!(reader.read_u8());
                    state = State::ParseMessageLength;
                },
                State::ParseMessageLength => {
                    self.length = try!(reader.read_u16::<NetworkByteOrder>());
                    state = State::ParseHandshakeType;
                },
                State::ParseHandshakeType => {
                    self.handshake_type = try!(reader.read_u8());
                    state = match self.handshake_type {
                        1 => State::ParseHandshakeLength,
                        _ => State::Fail("Wrong handshake type")
                    };
                },
                State::ParseHandshakeLength => {
                    self.handshake_length = try!(reader.read_uint::<NetworkByteOrder>(3)) as u32;
                    state = State::ParseHandshakeVersion;
                },
                State::ParseHandshakeVersion => {
                    self.client_hello.client_version.major = try!(reader.read_u8());
                    self.client_hello.client_version.minor = try!(reader.read_u8());
                    state = State::ParseRandomStruct;
                },
                State::ParseRandomStruct => {
                    let ref mut random = self.client_hello.random;
                    random.gmt_unix_time = try!(reader.read_u32::<NetworkByteOrder>());

                    for i in 0..(random.random_bytes.len()) {
                        random.random_bytes[i] = try!(reader.read_u8());
                    }

                    state = State::ParseSessionId;
                },
                State::ParseSessionId => {
                    self.session_id_length = try!(reader.read_u8());
                    for _ in 0..(self.session_id_length) {
                        self.client_hello.session_id.push(try!(reader.read_u8()));
                    }

                    state = State::ParseCipherSuites;
                },
                State::ParseCipherSuites => {
                    self.cipher_suites_length = try!(reader.read_u16::<NetworkByteOrder>());
                    for _ in 0..(self.cipher_suites_length / 2) {
                        let suite = CipherSuite(try!(reader.read_u8()), try!(reader.read_u8()));
                        self.client_hello.cipher_suites.push(suite);
                    }

                    state = State::ParseCompressionMethods;
                },
                State::ParseCompressionMethods => {
                    self.compression_methods_length = try!(reader.read_u8());
                    for _ in 0..(self.compression_methods_length) {
                        let method = CompressionMethod(try!(reader.read_u8()));
                        self.client_hello.compression_methods.push(method);
                    }

                    state = State::ParseExtensions;
                },
                State::ParseExtensions => {
                    self.extensions_length = try!(reader.read_u16::<NetworkByteOrder>());
                    let mut consumed = 0u16;

                    if self.extensions_length == 0 {
                        state = State::Done;
                        continue;
                    }

                    let mut extensions: Vec<Extension> = Vec::new();

                    while consumed != self.extensions_length {
                        let extension_type = try!(reader.read_u16::<NetworkByteOrder>());
                        consumed = consumed + 2;
                        let len = try!(reader.read_u16::<NetworkByteOrder>());
                        consumed = consumed + 2;

                        let mut extension_data: Vec<Opaque> = Vec::new();

                        for _ in 0..len {
                            extension_data.push(try!(reader.read_u8()));
                        }

                        consumed = consumed + len;

                        extensions.push(Extension {
                            extension_type: ExtensionType(extension_type),
                            length: len,
                            extension_data: extension_data
                        });
                    }

                    self.client_hello.extensions = extensions;
                    state = State::Done;
                },
                State::Done => {
                    return Ok(self.client_hello)
                },
                State::Fail(message) => {
                    return Err(io::Error::new(io::ErrorKind::Other, message));
                },
            }
        }
    }
}

/// Parser state
#[derive(Debug)]
enum State<'a> {
    /// Parsing has not started
    Initial,
    ParseProtocolVersion,
    ParseMessageLength,
    ParseHandshakeType,
    ParseHandshakeLength,
    ParseHandshakeVersion,
    ParseRandomStruct,
    ParseSessionId,
    ParseCipherSuites,
    ParseCompressionMethods,
    ParseExtensions,
    Done,
    /// Parsing has failed. A message is included.
    Fail(&'a str)
}

#[cfg(test)]
mod tests {
    use super::ClientHelloBuilder;
    use super::CONTENT_TYPE_HANDSHAKE;

    #[allow(unused_variables)]
    #[test]
    fn new_works() {
        let unused = ClientHelloBuilder::new();
    }

    #[test]
    fn wrong_content_type() {
        let parser = ClientHelloBuilder::new();
        let bytes = vec![0x15];

        if parser.parse_bytes(&bytes[..]).is_ok() {
            panic!("Expected parser to fail");
        }
    }

    #[test]
    fn incomplete_message() {
        let parser = ClientHelloBuilder::new();
        let bytes = vec![CONTENT_TYPE_HANDSHAKE];

        if parser.parse_bytes(&bytes[..]).is_ok() {
            panic!("Expected parser to fail");
        }
    }

    #[test]
    fn with_good_data() {
        // Hex dump from wireshark
        let data: Vec<u8> = vec![
            0x16, 0x03, 0x01, 0x01, 0x25, 0x01, 0x00, 0x01, 0x21, 0x03, 0x03, 0x73, 0x61, 0x2e,
            0x82, 0xaf, 0x80, 0x73, 0xa9, 0xb5, 0x7e, 0x28, 0xf8, 0x2a, 0x34, 0x7b, 0x2c, 0x3e,
            0xfe, 0x7c, 0x2c, 0x5f, 0xe6, 0x28, 0x7a, 0xd6, 0x1a, 0x35, 0x1b, 0x80, 0x81, 0x56,
            0xaf, 0x00, 0x00, 0x76, 0xc0, 0x30, 0xc0, 0x2c, 0xc0, 0x28, 0xc0, 0x24, 0xc0, 0x14,
            0xc0, 0x0a, 0x00, 0xa3, 0x00, 0x9f, 0x00, 0x6b, 0x00, 0x6a, 0x00, 0x39, 0x00, 0x38,
            0x00, 0x88, 0x00, 0x87, 0xc0, 0x32, 0xc0, 0x2e, 0xc0, 0x2a, 0xc0, 0x26, 0xc0, 0x0f,
            0xc0, 0x05, 0x00, 0x9d, 0x00, 0x3d, 0x00, 0x35, 0x00, 0x84, 0xc0, 0x12, 0xc0, 0x08,
            0x00, 0x16, 0x00, 0x13, 0xc0, 0x0d, 0xc0, 0x03, 0x00, 0x0a, 0xc0, 0x2f, 0xc0, 0x2b,
            0xc0, 0x27, 0xc0, 0x23, 0xc0, 0x13, 0xc0, 0x09, 0x00, 0xa2, 0x00, 0x9e, 0x00, 0x67,
            0x00, 0x40, 0x00, 0x33, 0x00, 0x32, 0x00, 0x9a, 0x00, 0x99, 0x00, 0x45, 0x00, 0x44,
            0xc0, 0x31, 0xc0, 0x2d, 0xc0, 0x29, 0xc0, 0x25, 0xc0, 0x0e, 0xc0, 0x04, 0x00, 0x9c,
            0x00, 0x3c, 0x00, 0x2f, 0x00, 0x96, 0x00, 0x41, 0x00, 0xff, 0x01, 0x00, 0x00, 0x82,
            0x00, 0x00, 0x00, 0x15, 0x00, 0x13, 0x00, 0x00, 0x10, 0x74, 0x65, 0x73, 0x74, 0x2e,
            0x65, 0x78, 0x61, 0x6d, 0x70, 0x6c, 0x65, 0x2e, 0x63, 0x6f, 0x6d, 0x00, 0x0b, 0x00,
            0x04, 0x03, 0x00, 0x01, 0x02, 0x00, 0x0a, 0x00, 0x34, 0x00, 0x32, 0x00, 0x0e, 0x00,
            0x0d, 0x00, 0x19, 0x00, 0x0b, 0x00, 0x0c, 0x00, 0x18, 0x00, 0x09, 0x00, 0x0a, 0x00,
            0x16, 0x00, 0x17, 0x00, 0x08, 0x00, 0x06, 0x00, 0x07, 0x00, 0x14, 0x00, 0x15, 0x00,
            0x04, 0x00, 0x05, 0x00, 0x12, 0x00, 0x13, 0x00, 0x01, 0x00, 0x02, 0x00, 0x03, 0x00,
            0x0f, 0x00, 0x10, 0x00, 0x11, 0x00, 0x0d, 0x00, 0x20, 0x00, 0x1e, 0x06, 0x01, 0x06,
            0x02, 0x06, 0x03, 0x05, 0x01, 0x05, 0x02, 0x05, 0x03, 0x04, 0x01, 0x04, 0x02, 0x04,
            0x03, 0x03, 0x01, 0x03, 0x02, 0x03, 0x03, 0x02, 0x01, 0x02, 0x02, 0x02, 0x03, 0x00,
            0x0f, 0x00, 0x01, 0x01];

        let parser = ClientHelloBuilder::new();
        let client_hello = parser.parse_bytes(&data[..]).unwrap();

        assert_eq!(client_hello.client_version.major, 3);
        assert_eq!(client_hello.client_version.minor, 3);
        assert_eq!(client_hello.random.gmt_unix_time, 0x73612e82);
        assert_eq!(client_hello.random.random_bytes, [0xaf, 0x80, 0x73, 0xa9, 0xb5, 0x7e, 0x28,
                   0xf8, 0x2a, 0x34, 0x7b, 0x2c, 0x3e, 0xfe, 0x7c, 0x2c, 0x5f, 0xe6, 0x28, 0x7a,
                   0xd6, 0x1a, 0x35, 0x1b, 0x80, 0x81, 0x56, 0xaf]);
        assert_eq!(client_hello.session_id.len(), 0);
        assert_eq!(client_hello.cipher_suites.len(), 59);
        assert_eq!(client_hello.compression_methods.len(), 1);
        assert_eq!(client_hello.extensions.len(), 5);
    }

    #[test]
    fn with_garbage_data_0() {
        let data: Vec<u8> = vec![
            0x16, 0x65, 0x78, 0x61, 0x6d, 0x70, 0x6c, 0x65, 0x2e, 0x63, 0x6f, 0x6d, 0x00, 0x0b,
            0x00, 0x04, 0x03, 0x00, 0x01, 0x02, 0x00, 0x0a, 0x00, 0x34, 0x00, 0x32, 0x00, 0x0e,
            0x00, 0x0d, 0x00, 0x19, 0x00, 0x0b, 0x00, 0x0c, 0x00, 0x18, 0x00, 0x09, 0x00, 0x0a,
            0x00, 0x16, 0x00, 0x17, 0x00, 0x08, 0x00, 0x06, 0x00, 0x07, 0x00, 0x14, 0x00, 0x15];
        let parser = ClientHelloBuilder::new();
        if parser.parse_bytes(&data[..]).is_ok() {
            panic!("Expected parser to fail");
        }
    }
}