
#[derive(Clone)]
pub struct RouteAssetItem {
    pub path: String,
    pub uses: Vec<crate::domain::AssetUse>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RouteAssetType {
    Scenery,
    Loft,
    Road,
    Track,
    TrackRules,
    Weather,
    Audio,
    Other,
}

impl RouteAssetType {
    pub fn label(self) -> &'static str {
        match self {
            Self::Scenery => "Сценери",
            Self::Loft => "Лофт",
            Self::Road => "Дорога",
            Self::Track => "Пути / Сигналы",
            Self::TrackRules => "Правило пути",
            Self::Weather => "Погода",
            Self::Audio => "Звук",
            Self::Other => "Прочее",
        }
    }
}

pub fn classify_route_asset_type(asset: &str, uses: &[crate::domain::AssetUse]) -> RouteAssetType {
    let mut track = false;
    let mut trackrules = false;
    let mut road = false;
    let mut loft = false;
    let mut weather = false;
    let mut audio = false;
    let mut scenery = false;

    for usage in uses {
        let source = usage.source_file.display().to_lowercase();
        if source.contains("trackrules") {
            trackrules = true;
        } else if source.contains("track tiles") || source.contains("tracks.bin") {
            track = true;
        } else if source.contains("road tiles") || source.contains("roads.bin") {
            road = true;
        } else if source.contains("loft tiles") || source.contains("lofts.bin") {
            loft = true;
        } else if source.contains("routeproperties.xml") {
            let asset_lower = asset.to_lowercase();
            if asset_lower.contains("sky")
                || asset_lower.contains("weather")
                || source.contains("weather")
            {
                weather = true;
            } else {
                scenery = true;
            }
        } else if source.contains("scenery") {
            scenery = true;
        }
        if source.contains("sounds") || source.contains("audio") {
            audio = true;
        }
    }

    let asset_lower = asset.to_lowercase();
    if asset_lower.ends_with(".wav")
        || asset_lower.ends_with(".ogg")
        || asset_lower.ends_with(".mp3")
    {
        audio = true;
    }

    if trackrules || asset_lower.contains("trackrules") {
        RouteAssetType::TrackRules
    } else if track {
        RouteAssetType::Track
    } else if road {
        RouteAssetType::Road
    } else if loft {
        RouteAssetType::Loft
    } else if weather {
        RouteAssetType::Weather
    } else if audio {
        RouteAssetType::Audio
    } else if scenery || asset_lower.contains("scenery") {
        RouteAssetType::Scenery
    } else if asset_lower.contains("track") {
        RouteAssetType::Track
    } else if asset_lower.contains("road") {
        RouteAssetType::Road
    } else if asset_lower.contains("loft") {
        RouteAssetType::Loft
    } else {
        RouteAssetType::Other
    }
}
pub fn is_protected_network_asset(asset: &RouteAssetItem) -> bool {
    if asset.uses.iter().any(|usage| {
        let path = usage.source_file.display().to_lowercase();
        path.contains("tracks.bin")
            || path.contains("/networks/")
            || path.contains("\\networks\\")
            || path.contains("trackrules")
            || path.contains("junctions")
    }) {
        return true;
    }

    let path_lower = asset.path.to_lowercase();
    if path_lower.contains("trackrules") || path_lower.contains("junctions") {
        return true;
    }

    if path_lower.contains("signal")
        && asset.uses.iter().any(|usage| {
            let path = usage.source_file.display().to_lowercase();
            path.contains("tracks.bin")
                || path.contains("/networks/")
                || path.contains("\\networks\\")
        })
    {
        return true;
    }

    false
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

