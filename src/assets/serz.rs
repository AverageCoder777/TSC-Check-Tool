use super::*;

#[derive(Debug, thiserror::Error)]
pub(crate) enum SerzError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("Invalid output: {0}")]
    InvalidOutput(String),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum AssetError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Message(String),
}

impl From<String> for AssetError {
    fn from(value: String) -> Self {
        Self::Message(value)
    }
}

impl From<AssetError> for String {
    fn from(value: AssetError) -> Self {
        value.to_string()
    }
}

pub(crate) fn read_serz_xml(asset_path: &Path) -> Result<String, SerzError> {
    let bytes = fs::read(asset_path).map_err(SerzError::Io)?;
    if !bytes.starts_with(b"SERZ") {
        return Ok(decode_xml_bytes(&bytes));
    }
    let mut reader = crate::serz::SerzReader::new(&bytes);
    let node = reader.parse().map_err(SerzError::InvalidOutput)?;
    Ok(crate::serz::SerzReader::to_xml(&node, 0, true))
}

pub(crate) fn decode_xml_bytes(bytes: &[u8]) -> String {
    if let Ok(text) = std::str::from_utf8(bytes)
        && !text.contains('\u{FFFD}')
    {
        return text.to_owned();
    }
    let (decoded, _) = encoding_rs::WINDOWS_1252.decode_without_bom_handling(bytes);
    decoded.into_owned()
}
