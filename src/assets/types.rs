use crate::common::domain::AssetUse;

#[derive(Clone)]
pub struct RouteAssetItem {
    pub path: String,
    pub uses: Vec<AssetUse>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
            Self::Scenery => "Scenery",
            Self::Loft => "Loft",
            Self::Road => "Road",
            Self::Track => "Track",
            Self::TrackRules => "Track Rules",
            Self::Weather => "Weather",
            Self::Audio => "Audio",
            Self::Other => "Other",
        }
    }
}

pub fn classify_route_asset_type(asset: &str, uses: &[AssetUse]) -> RouteAssetType {
    let asset_lower = asset.to_lowercase();

    let mut track = false;
    let mut trackrules = false;
    let mut road = false;
    let mut loft = false;
    let mut weather = false;
    let mut audio = false;
    let mut scenery = false;

    for usage in uses {
        let source = usage.source_file_search.as_str();
        if source.contains("trackrules") {
            trackrules = true;
        } else if source.contains("track tiles") || source.contains("tracks.bin") {
            track = true;
        } else if source.contains("road tiles") || source.contains("roads.bin") {
            road = true;
        } else if source.contains("loft tiles") || source.contains("lofts.bin") {
            loft = true;
        } else if source.contains("routeproperties.xml") {
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
    if asset
        .uses
        .iter()
        .any(|usage| usage.source_file.is_protected_pathway())
    {
        return true;
    }

    let path_lower = asset.path.to_lowercase();
    path_lower.contains("trackrules") || path_lower.contains("junctions")
}
