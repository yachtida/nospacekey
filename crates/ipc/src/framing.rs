use std::io::{self, Read, Write};

/// 1 フレーム本体の最大バイト数（16 MiB）。候補リストや変換結果は実用上これより遥かに小さい。
/// 長さ前置が壊れた/ストリームが desync した場合に巨大確保やハングへ陥らないための上限。
pub(crate) const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// 4byte リトルエンディアン長 + UTF-8 JSON 本体 を書き込む。
pub fn write_frame<W: Write, T: serde::Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let body = serde_json::to_vec(msg)?;
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
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds maximum {MAX_FRAME_LEN}"),
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

    #[test]
    fn request_roundtrip() {
        let msg = Request::Insert { session: 7, text: "nihongo".into(), style: None };
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let mut cur = std::io::Cursor::new(buf);
        let got: Request = read_frame(&mut cur).unwrap();
        assert_eq!(got, msg);
    }

    #[test]
    fn response_roundtrip() {
        let msg = Response::Candidates { candidates: vec!["日本語".into(), "にほんご".into()] };
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
}
