use std::io::{self, Read, Write};

/// サーバへ送る request 本体の上限。長さ前置だけで 16 MiB を確保させない。
pub const MAX_REQUEST_FRAME_LEN: usize = 256 * 1024;

/// response/generic frame 本体の上限。Rust client と Swift engine の wire 契約。
pub const MAX_RESPONSE_FRAME_LEN: usize = 16 * 1024 * 1024;

/// 既存利用者向けの別名。受信/generic frame は response 上限で検査する。
#[allow(dead_code)]
pub(crate) const MAX_FRAME_LEN: usize = MAX_RESPONSE_FRAME_LEN;

fn frame_length_error(kind: io::ErrorKind, len: usize, max: usize) -> io::Error {
    io::Error::new(kind, format!("frame length {len} exceeds maximum {max}"))
}

fn serialize_frame<T: serde::Serialize>(
    msg: &T,
    max: usize,
    kind: io::ErrorKind,
) -> io::Result<Vec<u8>> {
    let body = serde_json::to_vec(msg)?;
    if body.len() > max || u32::try_from(body.len()).is_err() {
        return Err(frame_length_error(kind, body.len(), max));
    }
    Ok(body)
}

/// 4byte リトルエンディアン長 + UTF-8 JSON 本体 を書き込む。
/// generic frame は response 上限（16 MiB）で検査する。
pub fn write_frame<W: Write, T: serde::Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let body = serialize_frame(msg, MAX_RESPONSE_FRAME_LEN, io::ErrorKind::InvalidInput)?;
    let len = (body.len() as u32).to_le_bytes();
    w.write_all(&len)?;
    w.write_all(&body)?;
    w.flush()
}

/// request 専用 writer。serialize 後に 256 KiB を超えていたら、長さ prefix を含め
/// 1 byte も writer へ渡さず InvalidInput を返す。
pub fn write_request_frame<W: Write, T: serde::Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let body = serialize_frame(msg, MAX_REQUEST_FRAME_LEN, io::ErrorKind::InvalidInput)?;
    let len = (body.len() as u32).to_le_bytes();
    w.write_all(&len)?;
    w.write_all(&body)?;
    w.flush()
}

/// 1フレーム読み取り、JSON をデシリアライズする。
pub fn read_frame<R: Read, T: serde::de::DeserializeOwned>(r: &mut R) -> io::Result<T> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_RESPONSE_FRAME_LEN {
        return Err(frame_length_error(
            io::ErrorKind::InvalidData,
            len,
            MAX_RESPONSE_FRAME_LEN,
        ));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Request, Response};

    struct CountingWriter {
        writes: usize,
    }
    impl std::io::Write for CountingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.writes += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn request_roundtrip() {
        let msg = Request::Insert {
            session: 7,
            text: "nihongo".into(),
            style: None,
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let got: Request = read_frame(&mut cur).unwrap();
        assert_eq!(got, msg);
    }

    #[test]
    fn response_roundtrip() {
        let msg = Response::Candidates {
            candidates: vec!["日本語".into(), "にほんご".into()],
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let got: Response = read_frame(&mut cur).unwrap();
        assert_eq!(got, msg);
    }

    #[test]
    fn oversized_length_is_rejected_without_allocating() {
        // 長さ前置が上限超（ここでは u32::MAX）の壊れたフレーム。巨大確保やハングに陥らず
        // InvalidData で即エラーになること（body は1バイトも読まない）。
        let mut bytes = u32::MAX.to_le_bytes().to_vec();
        bytes.extend_from_slice(b"\x00"); // body は来ない想定だが read 前に弾かれる
        let mut cur = std::io::Cursor::new(bytes);
        let err = read_frame::<_, Request>(&mut cur).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn request_limit_is_distinct_from_response_limit() {
        assert_eq!(MAX_REQUEST_FRAME_LEN, 256 * 1024);
        assert_eq!(MAX_RESPONSE_FRAME_LEN, 16 * 1024 * 1024);
    }

    #[test]
    fn request_writer_rejects_oversize_before_writing_prefix() {
        let oversized = "x".repeat(MAX_REQUEST_FRAME_LEN);
        let mut out = CountingWriter { writes: 0 };
        let err = write_request_frame(&mut out, &oversized).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(out.writes, 0);
    }
}
