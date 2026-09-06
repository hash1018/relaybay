//! AMF0: the value encoding RTMP writes its commands and metadata in.
//!
//! A command message is a sequence of AMF0 values laid end to end — a name,
//! a transaction number, and whatever arguments the call takes:
//!
//! ```text
//! "connect", 1.0, { app: "live", tcUrl: "rtmp://host/live", … }
//! ```
//!
//! So reading one is reading values until the payload runs out, which is
//! what [`read_all`] does. Nothing about this is specific to RTMP; AMF0 is
//! Flash's general object encoding and RTMP is one of the things that
//! carries it.
//!
//! # Why an unknown type is refused rather than passed through
//!
//! [`crate::codec::h264`] keeps NAL units it does not recognize, on the
//! principle that a relay forwards what it does not understand. That cannot
//! be done here, and the difference is worth stating because it looks like
//! an inconsistency.
//!
//! A NAL unit's length is decided by whatever framed it, so an unrecognized
//! one can be handed on whole without being understood. An AMF0 value's
//! length is decided by *its own type marker*: a number is eight bytes, a
//! string is two more than the two-byte count in front of it, an object runs
//! to a terminator. A marker this does not know is therefore a value of
//! unknown length, and the next value could begin anywhere. There is nothing
//! to forward and nothing to skip, so reading stops.
//!
//! # What is not read
//!
//! References, movie clips, record sets, XML documents, typed objects and
//! the marker that switches to AMF3. None of them appear in an RTMP session
//! between an encoder and a server, and each is refused by name rather than
//! as an unknown marker, so that if one ever does turn up the log says which
//! it was.

use bytes::{BufMut, BytesMut};

/// How deeply objects and arrays may nest. RTMP's own messages go two deep;
/// the limit is here because reading is recursive and a peer that sent
/// nothing but object markers would otherwise run the stack out.
pub const MAX_DEPTH: usize = 32;

/// The type markers, in the order the specification numbers them.
mod marker {
    pub const NUMBER: u8 = 0x00;
    pub const BOOLEAN: u8 = 0x01;
    pub const STRING: u8 = 0x02;
    pub const OBJECT: u8 = 0x03;
    pub const MOVIE_CLIP: u8 = 0x04;
    pub const NULL: u8 = 0x05;
    pub const UNDEFINED: u8 = 0x06;
    pub const REFERENCE: u8 = 0x07;
    pub const ECMA_ARRAY: u8 = 0x08;
    pub const OBJECT_END: u8 = 0x09;
    pub const STRICT_ARRAY: u8 = 0x0a;
    pub const DATE: u8 = 0x0b;
    pub const LONG_STRING: u8 = 0x0c;
    pub const UNSUPPORTED: u8 = 0x0d;
    pub const RECORD_SET: u8 = 0x0e;
    pub const XML_DOCUMENT: u8 = 0x0f;
    pub const TYPED_OBJECT: u8 = 0x10;
    pub const AVM_PLUS: u8 = 0x11;
}

/// One AMF0 value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// Every number AMF0 has, including the ones a caller means as integers:
    /// a transaction id and a stream id both arrive as doubles.
    Number(f64),
    Boolean(bool),
    /// Written short or long depending on its length, which is a detail of
    /// the encoding and not of the value, so both read back to this.
    String(String),
    /// An anonymous object.
    Object(Vec<(String, Value)>),
    /// An associative array, which is an object with a count in front of it.
    /// Kept apart from [`Value::Object`] because `onMetaData` arrives as one
    /// and some players will not read it back as the other.
    EcmaArray(Vec<(String, Value)>),
    /// A dense array, whose members have positions rather than names.
    StrictArray(Vec<Value>),
    /// Milliseconds since the Unix epoch. The time zone that follows it on
    /// the wire is required to be zero and is ignored by everything, so it
    /// is neither kept nor asked about.
    Date(f64),
    Null,
    Undefined,
}

impl Value {
    /// The first property named `key`, for the two object-shaped variants.
    ///
    /// First rather than last where a key repeats. AMF0 has no rule about
    /// duplicates and nothing that speaks RTMP produces them; taking the
    /// first is only a decision so that there is one.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.properties()?
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }

    /// The properties, for the two object-shaped variants.
    pub fn properties(&self) -> Option<&[(String, Value)]> {
        match self {
            Self::Object(properties) | Self::EcmaArray(properties) => Some(properties),
            _ => None,
        }
    }

    /// The text, for a string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(text) => Some(text),
            _ => None,
        }
    }

    /// The number, for a number.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Number(number) => Some(*number),
            _ => None,
        }
    }

    /// The flag, for a boolean.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Boolean(flag) => Some(*flag),
            _ => None,
        }
    }
}

/// What can be wrong with an AMF0 payload.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Amf0Error {
    /// A value ran past the end of the payload.
    #[error("truncated at byte {offset}: {needed} more byte(s) needed")]
    Truncated { offset: usize, needed: usize },

    /// A type marker this does not know, which is a value of unknown length
    /// — see the module docs on why that ends the read.
    #[error("unknown type marker {marker:#04x} at byte {offset}")]
    UnknownMarker { marker: u8, offset: usize },

    /// A type this does not read, named so that a log says which.
    #[error("{name} at byte {offset} is a type this does not read")]
    UnreadType { name: &'static str, offset: usize },

    /// AMF0 strings are UTF-8 and this one is not. Refused rather than
    /// replaced, because every string RTMP carries is something a session is
    /// routed by — a command name, a property key, a stream path — and a
    /// lossy one silently sends the stream somewhere else.
    #[error("the string at byte {offset} is not UTF-8")]
    NotUtf8 { offset: usize },

    /// Objects nested past [`MAX_DEPTH`].
    #[error("values nested more than {limit} deep")]
    TooDeep { limit: usize },

    /// An object's terminator turned up where a value was expected, which
    /// means the object it would have closed was never opened.
    #[error("an object end marker at byte {offset} with no object open")]
    UnexpectedObjectEnd { offset: usize },

    /// A string too long for even the four-byte count of the long form.
    #[error("a string of {length} bytes is longer than AMF0 can express")]
    StringTooLong { length: usize },

    /// A property key of no bytes, which would read back as the terminator
    /// that ends the object, or one too long for its two-byte count.
    #[error("a property key of {length} bytes cannot be written")]
    UnwritableKey { length: usize },
}

/// Reads every value in `data`, in order.
///
/// A command message holds several — a name, a transaction number and its
/// arguments — with nothing between them and no count in front, so the end
/// of the payload is the only thing that says how many there are.
pub fn read_all(data: &[u8]) -> Result<Vec<Value>, Amf0Error> {
    let mut reader = Reader {
        data,
        pos: 0,
        depth: 0,
    };
    let mut values = Vec::new();
    while reader.pos < data.len() {
        values.push(reader.value()?);
    }
    Ok(values)
}

/// Writes values one after another, appending to `out`.
pub fn write_all(out: &mut BytesMut, values: &[Value]) -> Result<(), Amf0Error> {
    for value in values {
        write(out, value)?;
    }
    Ok(())
}

/// Writes one value, appending to `out`.
///
/// Round-trips by value rather than by bytes: a short string encoded in the
/// long form reads back the same and goes out in the short one, because the
/// choice between them is a property of the encoding and not of the string.
pub fn write(out: &mut BytesMut, value: &Value) -> Result<(), Amf0Error> {
    match value {
        Value::Number(number) => {
            out.put_u8(marker::NUMBER);
            out.put_f64(*number);
        }
        Value::Boolean(flag) => {
            out.put_u8(marker::BOOLEAN);
            out.put_u8(u8::from(*flag));
        }
        Value::String(text) => put_string(out, text)?,
        Value::Object(properties) => {
            out.put_u8(marker::OBJECT);
            put_properties(out, properties)?;
        }
        Value::EcmaArray(properties) => {
            out.put_u8(marker::ECMA_ARRAY);
            // The count no reader is supposed to trust. Written truthfully
            // anyway, for the readers that do.
            out.put_u32(properties.len() as u32);
            put_properties(out, properties)?;
        }
        Value::StrictArray(values) => {
            out.put_u8(marker::STRICT_ARRAY);
            out.put_u32(values.len() as u32);
            for value in values {
                write(out, value)?;
            }
        }
        Value::Date(millis) => {
            out.put_u8(marker::DATE);
            out.put_f64(*millis);
            // The time zone, which the specification requires to be zero.
            out.put_i16(0);
        }
        Value::Null => out.put_u8(marker::NULL),
        Value::Undefined => out.put_u8(marker::UNDEFINED),
    }
    Ok(())
}

fn put_string(out: &mut BytesMut, text: &str) -> Result<(), Amf0Error> {
    let bytes = text.as_bytes();
    match u16::try_from(bytes.len()) {
        Ok(length) => {
            out.put_u8(marker::STRING);
            out.put_u16(length);
        }
        Err(_) => {
            let length = u32::try_from(bytes.len()).map_err(|_| Amf0Error::StringTooLong {
                length: bytes.len(),
            })?;
            out.put_u8(marker::LONG_STRING);
            out.put_u32(length);
        }
    }
    out.put_slice(bytes);
    Ok(())
}

fn put_properties(out: &mut BytesMut, properties: &[(String, Value)]) -> Result<(), Amf0Error> {
    for (key, value) in properties {
        let bytes = key.as_bytes();
        // A key of no bytes is what ends an object, so writing one would
        // truncate everything after it into something that still parses.
        let length = u16::try_from(bytes.len())
            .ok()
            .filter(|length| *length > 0)
            .ok_or(Amf0Error::UnwritableKey {
                length: bytes.len(),
            })?;
        out.put_u16(length);
        out.put_slice(bytes);
        write(out, value)?;
    }
    // The terminator: an empty key, then the marker that confirms it.
    out.put_u16(0);
    out.put_u8(marker::OBJECT_END);
    Ok(())
}

/// Bounds-checked forward reading, so a malformed payload fails at the field
/// that is wrong rather than by panicking somewhere after it.
struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
    depth: usize,
}

impl<'a> Reader<'a> {
    fn need(&self, count: usize) -> Result<(), Amf0Error> {
        if self.pos + count > self.data.len() {
            return Err(Amf0Error::Truncated {
                offset: self.pos,
                needed: self.pos + count - self.data.len(),
            });
        }
        Ok(())
    }

    fn bytes(&mut self, count: usize) -> Result<&'a [u8], Amf0Error> {
        self.need(count)?;
        // Copied out of the field so the slice borrows the payload rather
        // than the reader, which lets a caller hold it while reading on.
        let data = self.data;
        self.pos += count;
        Ok(&data[self.pos - count..self.pos])
    }

    fn u8(&mut self) -> Result<u8, Amf0Error> {
        Ok(self.bytes(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, Amf0Error> {
        let bytes = self.bytes(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u32(&mut self) -> Result<u32, Amf0Error> {
        let bytes = self.bytes(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn f64(&mut self) -> Result<f64, Amf0Error> {
        let bytes = self.bytes(8)?;
        Ok(f64::from_be_bytes(bytes.try_into().expect("eight bytes")))
    }

    fn string(&mut self, length: usize) -> Result<String, Amf0Error> {
        let offset = self.pos;
        let bytes = self.bytes(length)?;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| Amf0Error::NotUtf8 { offset })
    }

    /// Runs `read` one level further in, refusing to go past [`MAX_DEPTH`].
    fn nested<T>(
        &mut self,
        read: impl FnOnce(&mut Self) -> Result<T, Amf0Error>,
    ) -> Result<T, Amf0Error> {
        if self.depth >= MAX_DEPTH {
            return Err(Amf0Error::TooDeep { limit: MAX_DEPTH });
        }
        self.depth += 1;
        let value = read(self);
        self.depth -= 1;
        value
    }

    fn value(&mut self) -> Result<Value, Amf0Error> {
        let offset = self.pos;
        let unread = |name| Err(Amf0Error::UnreadType { name, offset });
        match self.u8()? {
            marker::NUMBER => Ok(Value::Number(self.f64()?)),
            // Anything but zero is true. Encoders write 1, but a reader that
            // insisted on it would refuse a stream over a value it can read.
            marker::BOOLEAN => Ok(Value::Boolean(self.u8()? != 0)),
            marker::STRING => {
                let length = usize::from(self.u16()?);
                Ok(Value::String(self.string(length)?))
            }
            marker::LONG_STRING => {
                let length = self.u32()? as usize;
                Ok(Value::String(self.string(length)?))
            }
            marker::OBJECT => Ok(Value::Object(self.properties()?)),
            marker::ECMA_ARRAY => {
                // The count, which the specification says is unreliable and
                // which implementations do write as zero. Read to move past
                // it and then ignored: the terminator is what says where the
                // array ends.
                self.u32()?;
                Ok(Value::EcmaArray(self.properties()?))
            }
            marker::STRICT_ARRAY => {
                let count = self.u32()?;
                self.nested(|reader| {
                    // Not `with_capacity(count)`: the count is a number the
                    // peer chose, and honouring it would let four header
                    // bytes ask for gigabytes. Growing as values arrive
                    // makes it send them first.
                    let mut values = Vec::new();
                    for _ in 0..count {
                        values.push(reader.value()?);
                    }
                    Ok(Value::StrictArray(values))
                })
            }
            marker::DATE => {
                let millis = self.f64()?;
                // The time zone, required to be zero and ignored everywhere.
                self.bytes(2)?;
                Ok(Value::Date(millis))
            }
            marker::NULL => Ok(Value::Null),
            marker::UNDEFINED => Ok(Value::Undefined),
            marker::OBJECT_END => Err(Amf0Error::UnexpectedObjectEnd { offset }),
            marker::MOVIE_CLIP => unread("a movie clip"),
            marker::REFERENCE => unread("a reference"),
            marker::UNSUPPORTED => unread("an unsupported-type marker"),
            marker::RECORD_SET => unread("a record set"),
            marker::XML_DOCUMENT => unread("an XML document"),
            marker::TYPED_OBJECT => unread("a typed object"),
            marker::AVM_PLUS => unread("a switch to AMF3"),
            marker => Err(Amf0Error::UnknownMarker { marker, offset }),
        }
    }

    /// The body both object-shaped types share: named values until an empty
    /// key and the marker that confirms it.
    fn properties(&mut self) -> Result<Vec<(String, Value)>, Amf0Error> {
        self.nested(|reader| {
            let mut properties = Vec::new();
            loop {
                let length = usize::from(reader.u16()?);
                if length == 0 {
                    let offset = reader.pos;
                    let marker = reader.u8()?;
                    if marker != marker::OBJECT_END {
                        return Err(Amf0Error::UnknownMarker { marker, offset });
                    }
                    return Ok(properties);
                }
                let key = reader.string(length)?;
                properties.push((key, reader.value()?));
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string(text: &str) -> Value {
        Value::String(text.to_owned())
    }

    fn object(properties: &[(&str, Value)]) -> Value {
        Value::Object(
            properties
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone()))
                .collect(),
        )
    }

    fn round_trip(values: &[Value]) -> Vec<Value> {
        let mut out = BytesMut::new();
        write_all(&mut out, values).expect("writable");
        read_all(&out).expect("readable")
    }

    #[test]
    fn every_value_round_trips() {
        let values = vec![
            Value::Number(1.0),
            Value::Number(-0.5),
            Value::Boolean(true),
            Value::Boolean(false),
            string("connect"),
            string(""),
            Value::Null,
            Value::Undefined,
            Value::Date(1_700_000_000_000.0),
            Value::StrictArray(vec![Value::Number(1.0), string("two")]),
            Value::EcmaArray(vec![("duration".to_owned(), Value::Number(0.0))]),
            object(&[
                ("app", string("live")),
                ("audioCodecs", Value::Number(4071.0)),
                ("nested", object(&[("deep", Value::Boolean(true))])),
            ]),
        ];
        assert_eq!(round_trip(&values), values);
    }

    #[test]
    fn an_object_with_no_properties_round_trips() {
        let values = vec![object(&[]), Value::EcmaArray(Vec::new())];
        assert_eq!(round_trip(&values), values);
    }

    #[test]
    fn properties_keep_the_order_they_were_written_in() {
        let value = object(&[
            ("z", Value::Number(1.0)),
            ("a", Value::Number(2.0)),
            ("m", Value::Number(3.0)),
        ]);
        let read = round_trip(&[value]);
        let keys: Vec<_> = read[0]
            .properties()
            .expect("an object")
            .iter()
            .map(|(key, _)| key.as_str())
            .collect();
        assert_eq!(keys, ["z", "a", "m"]);
    }

    /// Bytes written by hand rather than by [`write`], so that the reader is
    /// checked against the encoding and not against this module's own idea
    /// of it.
    #[test]
    fn a_connect_command_reads_as_the_values_it_is_made_of() {
        let payload = [
            // "connect"
            &[0x02, 0x00, 0x07][..],
            b"connect",
            // 1.0
            &[0x00, 0x3f, 0xf0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
            // { app: "live", fpad: false }
            &[0x03],
            &[0x00, 0x03],
            b"app",
            &[0x02, 0x00, 0x04],
            b"live",
            &[0x00, 0x04],
            b"fpad",
            &[0x01, 0x00],
            &[0x00, 0x00, 0x09],
        ]
        .concat();

        let values = read_all(&payload).expect("a well-formed command");
        assert_eq!(values[0].as_str(), Some("connect"));
        assert_eq!(values[1].as_f64(), Some(1.0));
        assert_eq!(values[2].get("app").and_then(Value::as_str), Some("live"));
        assert_eq!(values[2].get("fpad").and_then(Value::as_bool), Some(false));
        assert_eq!(values[2].get("absent"), None);
        assert_eq!(values.len(), 3);
    }

    #[test]
    fn an_ecma_array_ends_where_its_terminator_is_and_not_where_its_count_says() {
        // A count of 99 with two properties in it, which is what an encoder
        // that writes the count wrongly — or as zero — produces.
        let payload = [
            &[0x08, 0x00, 0x00, 0x00, 0x63][..],
            &[0x00, 0x08],
            b"duration",
            &[0x00, 0x40, 0x24, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
            &[0x00, 0x05],
            b"width",
            &[0x00, 0x40, 0x94, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
            &[0x00, 0x00, 0x09],
            // A value after the array, which is only reached if the array
            // stopped at its terminator.
            &[0x05],
        ]
        .concat();

        let values = read_all(&payload).expect("a well-formed array");
        assert_eq!(values.len(), 2);
        assert_eq!(
            values[0].get("duration").and_then(Value::as_f64),
            Some(10.0)
        );
        assert_eq!(values[0].get("width").and_then(Value::as_f64), Some(1280.0));
        assert_eq!(values[1], Value::Null);
    }

    #[test]
    fn a_string_too_long_for_a_two_byte_count_is_written_long_and_read_back() {
        let long = "x".repeat(usize::from(u16::MAX) + 1);
        let mut out = BytesMut::new();
        write(&mut out, &string(&long)).unwrap();
        assert_eq!(out[0], marker::LONG_STRING);
        assert_eq!(read_all(&out).unwrap(), vec![string(&long)]);
    }

    #[test]
    fn a_strict_array_that_promises_more_than_it_sends_is_refused() {
        // Four bytes claiming four billion members, and nothing after them.
        let payload = [0x0a, 0xff, 0xff, 0xff, 0xff];
        assert_eq!(
            read_all(&payload),
            Err(Amf0Error::Truncated {
                offset: 5,
                needed: 1
            })
        );
    }

    #[test]
    fn a_value_cut_short_is_refused_at_the_field_that_is_wrong() {
        // A number needs eight bytes and three are here.
        assert_eq!(
            read_all(&[0x00, 0x3f, 0xf0, 0x00]),
            Err(Amf0Error::Truncated {
                offset: 1,
                needed: 5
            })
        );
        // A string that says seven bytes and carries four.
        assert_eq!(
            read_all(&[0x02, 0x00, 0x07, b'c', b'o', b'n', b'n']),
            Err(Amf0Error::Truncated {
                offset: 3,
                needed: 3
            })
        );
    }

    #[test]
    fn an_object_that_never_closes_is_refused() {
        let payload = [&[0x03, 0x00, 0x03][..], b"app", &[0x05]].concat();
        assert!(matches!(
            read_all(&payload),
            Err(Amf0Error::Truncated { .. })
        ));
    }

    #[test]
    fn an_unknown_marker_is_refused() {
        assert_eq!(
            read_all(&[0x05, 0x2a]),
            Err(Amf0Error::UnknownMarker {
                marker: 0x2a,
                offset: 1
            })
        );
    }

    #[test]
    fn a_type_this_does_not_read_says_which_it_was() {
        assert_eq!(
            read_all(&[marker::AVM_PLUS]),
            Err(Amf0Error::UnreadType {
                name: "a switch to AMF3",
                offset: 0
            })
        );
        assert_eq!(
            read_all(&[marker::TYPED_OBJECT]),
            Err(Amf0Error::UnreadType {
                name: "a typed object",
                offset: 0
            })
        );
    }

    #[test]
    fn a_terminator_with_no_object_open_is_refused() {
        assert_eq!(
            read_all(&[marker::OBJECT_END]),
            Err(Amf0Error::UnexpectedObjectEnd { offset: 0 })
        );
    }

    #[test]
    fn a_string_that_is_not_utf8_is_refused() {
        assert_eq!(
            read_all(&[0x02, 0x00, 0x02, 0xff, 0xfe]),
            Err(Amf0Error::NotUtf8 { offset: 3 })
        );
    }

    #[test]
    fn values_nested_past_the_limit_are_refused() {
        // An object holding one property named "k" whose value is another,
        // per level, and nothing that closes any of them: the depth is
        // reached before the truncation is.
        let payload = [marker::OBJECT, 0x00, 0x01, b'k'].repeat(MAX_DEPTH + 1);
        assert_eq!(
            read_all(&payload),
            Err(Amf0Error::TooDeep { limit: MAX_DEPTH })
        );
    }

    #[test]
    fn nesting_up_to_the_limit_is_read() {
        let mut value = Value::Null;
        for _ in 0..MAX_DEPTH {
            value = Value::Object(vec![("k".to_owned(), value)]);
        }
        assert_eq!(round_trip(&[value.clone()]), vec![value]);
    }

    #[test]
    fn a_key_that_would_read_back_as_a_terminator_is_refused() {
        let value = Value::Object(vec![(String::new(), Value::Null)]);
        let mut out = BytesMut::new();
        assert_eq!(
            write(&mut out, &value),
            Err(Amf0Error::UnwritableKey { length: 0 })
        );
    }

    #[test]
    fn an_empty_payload_holds_no_values() {
        assert_eq!(read_all(&[]), Ok(Vec::new()));
    }
}
