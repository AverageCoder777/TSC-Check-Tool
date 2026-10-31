//! Разбор ссылок на ассеты внутри XML RailWorks.
//!
//! Ссылка выглядит так: `<Provider>/<Product>/<путь>`. В маршрутах она может
//! быть без расширения и с суффиксом варианта `[00]`, а в файлах сценарных
//! поездов — с указанием на объект поезда.

use std::path::Path;

pub(crate) struct AssetReference {
    pub(crate) path: String,
    pub(crate) provider_range: std::ops::Range<usize>,
    pub(crate) product_range: std::ops::Range<usize>,
    pub(crate) blueprint_range: std::ops::Range<usize>,
    /// Что за объект стоит по ссылкой: объект маршрута или файл состава.
    pub(crate) kind: crate::domain::AssetKind,
}
/// Сколько символов отличается в двух именах. ///
pub(crate) fn capture_value_range(capture: &regex::Match<'_>) -> std::ops::Range<usize> {
    let value = capture.as_str();
    let leading = value.len() - value.trim_start().len();
    let trimmed = value.trim();
    capture.start() + leading..capture.start() + leading + trimmed.len()
}
/// Ищет первое вхождение подстроки без учёта регистра ASCII.
/// /// Нужен для ручного разбора XML: регулярное выражение с ленивым `.*?` заново
/// просматривает остаток файла для каждого найденного блока, из-за чего разбор
/// маршрута дорожает квадратично.
pub(crate) fn find_ignore_ascii_case(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let first = needle[0];
    let last = haystack.len() - needle.len();
    let mut index = from;
    while index <= last {
        let offset = haystack[index..=last]
            .iter()
            .position(|byte| *byte == first)?;
        let start = index + offset;
        if haystack[start..start + needle.len()].eq_ignore_ascii_case(needle) {
            return Some(start);
        }
        index = start + 1;
    }
    None
}
/// Ищет элемент `<Тег ...>значение</Тег>` начиная с заданного места.
/// /// Возвращает диапазон значения без окружающих пробелов и позицию сразу за
/// закрывающим тегом. Значение не может содержать `<`, как и в исходном
/// шаблоне `([^<]*)`.
pub(crate) fn parse_tagged_value(
    haystack: &[u8],
    tag: &str,
    start: usize,
) -> Option<(std::ops::Range<usize>, usize)> {
    let tag_bytes = tag.as_bytes();
    let after_open = start.checked_add(1)?;
    if haystack.get(start)? != &b'<' {
        return None;
    }
    if !haystack
        .get(after_open..after_open.checked_add(tag_bytes.len())?)?
        .eq_ignore_ascii_case(tag_bytes)
    {
        return None;
    }
    let tag_start = after_open + tag_bytes.len();
    let tag_end = tag_start
        + haystack
            .get(tag_start..)?
            .iter()
            .position(|byte| *byte == b'>')?;
    let value_start = tag_end + 1;
    let value_end = value_start
        + haystack
            .get(value_start..)?
            .iter()
            .position(|byte| *byte == b'<')?;
    if haystack.get(value_end)? != &b'<' || haystack.get(value_end + 1)? != &b'/' {
        return None;
    }
    let close_tag_start = value_end + 2;
    if !haystack
        .get(close_tag_start..close_tag_start.checked_add(tag_bytes.len())?)?
        .eq_ignore_ascii_case(tag_bytes)
    {
        return None;
    }
    let close_end = close_tag_start + tag_bytes.len();
    if haystack.get(close_end)? != &b'>' {
        return None;
    }
    let value = &haystack[value_start..value_end];
    let leading = value
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(value.len());
    let trimmed = value
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map(|last| last + 1)
        .unwrap_or(leading);
    Some((value_start + leading..value_start + trimmed, close_end + 1))
}
/// Позиция следующего открывающего тега после заданного места.
pub(crate) fn next_tag_start(haystack: &[u8], from: usize) -> Option<usize> {
    haystack
        .get(from..)?
        .iter()
        .position(|byte| *byte == b'<')
        .map(|offset| from + offset)
}
/// Ищет элемент по имени в заданном диапазоне файла.
pub(crate) fn find_tagged_value(
    haystack: &[u8],
    tag: &str,
    from: usize,
    limit: usize,
) -> Option<(std::ops::Range<usize>, usize)> {
    let mut cursor = from;
    while cursor < limit {
        let position = next_tag_start(haystack, cursor)?;
        if position >= limit {
            return None;
        }
        if let Some((range, end)) = parse_tagged_value(haystack, tag, position)
            && end <= limit
        {
            return Some((range, end));
        }
        cursor = position + 1;
    }
    None
}
/// Блоки `iBlueprintLibrary-cAbsoluteBlueprintID` с границами разметки.
pub(crate) fn absolute_blueprint_blocks(haystack: &[u8]) -> Vec<(usize, usize, usize, usize)> {
    let open = b"<iBlueprintLibrary-cAbsoluteBlueprintID>";
    let close = b"</iBlueprintLibrary-cAbsoluteBlueprintID>";
    let mut blocks = Vec::new();
    let mut cursor = 0usize;
    while let Some(open_at) = find_ignore_ascii_case(haystack, open, cursor) {
        let content_start = open_at + open.len();
        let Some(close_at) = find_ignore_ascii_case(haystack, close, content_start) else {
            break;
        };
        blocks.push((open_at, close_at + close.len(), content_start, close_at));
        cursor = close_at + close.len();
    }
    blocks
}

pub(crate) fn asset_relative_path(
    provider: &str,
    product: &str,
    blueprint: &str,
) -> Option<String> {
    let blueprint_path = Path::new(blueprint);
    let extension = blueprint_path.extension()?;
    let file_name = if extension.eq_ignore_ascii_case("xml") {
        blueprint_path.with_extension("bin")
    } else if extension.eq_ignore_ascii_case("bin") {
        blueprint_path.to_path_buf()
    } else {
        return None;
    };
    let relative_path = Path::new(provider).join(product).join(file_name);
    let components: Vec<_> = relative_path.components().collect();
    if components.is_empty()
        || components
            .iter()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return None;
    }
    Some(relative_path.to_string_lossy().replace('\\', "/"))
}

pub(crate) fn build_asset_reference(
    provider: &str,
    product: &str,
    blueprint: &str,
    provider_range: std::ops::Range<usize>,
    product_range: std::ops::Range<usize>,
    blueprint_range: std::ops::Range<usize>,
    kind: crate::domain::AssetKind,
) -> Option<AssetReference> {
    let provider = crate::translit::decode_xml_text(provider.trim());
    let product = crate::translit::decode_xml_text(product.trim());
    let blueprint = crate::translit::decode_xml_text(blueprint.trim());
    if provider.is_empty() || product.is_empty() || blueprint.is_empty() {
        return None;
    }
    let path = asset_relative_path(&provider, &product, &blueprint)?;
    Some(AssetReference {
        path,
        provider_range,
        product_range,
        blueprint_range,
        kind,
    })
}
/// Собирает все ссылки на ассеты маршрута.
/// /// Основной источник — блоки `iBlueprintLibrary-cAbsoluteBlueprintID`, внутри которых
/// Provider, Product и путь к блюпринту принадлежат одной ссылке. Такой разбор не даёт
/// пустым заготовкам `ReskinBlueprintID` «съесть» ссылку следующего за ними объекта.
/// /// Разбор идёт вручную, без регулярных выражений с ленивым `.*?`: такие шаблоны
/// просматривают остаток файла заново для каждого блока и делают проверку маршрута
/// квадратичной по размеру XML.
pub(crate) fn extract_asset_references_with_locations(xml: &str) -> Vec<AssetReference> {
    let haystack = xml.as_bytes();
    let mut references: Vec<(usize, AssetReference)> = Vec::new();
    let mut covered: Vec<std::ops::Range<usize>> = Vec::new();
    // Контекст (scenery/loft/vehicle) определяется ближайшим предыдущим тегом.
    // Чтобы не делать O(N) rfind для каждого блока на гигантском Tracks.bin,
    // поддерживаем индексы последних тегов инкрементально.
    let mut context_cursor = 0usize;
    let mut vehicle_ctx: Option<usize> = None;
    let mut scenery_ctx: Option<usize> = None;
    let mut loft_ctx: Option<usize> = None;
    for (open_at, block_end, content_start, content_end) in absolute_blueprint_blocks(haystack) {
        update_context_state(
            xml,
            &mut context_cursor,
            open_at,
            &mut vehicle_ctx,
            &mut scenery_ctx,
            &mut loft_ctx,
        );
        covered.push(open_at..block_end);
        let Some((provider_range, _)) =
            find_tagged_value(haystack, "Provider", content_start, content_end)
        else {
            continue;
        };
        let Some((product_range, _)) =
            find_tagged_value(haystack, "Product", content_start, content_end)
        else {
            continue;
        };
        let Some((blueprint_range, _)) =
            find_tagged_value(haystack, "BlueprintID", content_start, content_end)
        else {
            continue;
        };
        let Some(reference) = build_asset_reference(
            &xml[provider_range.clone()],
            &xml[product_range.clone()],
            &xml[blueprint_range.clone()],
            provider_range,
            product_range,
            blueprint_range,
            kind_from_context_state(&vehicle_ctx, &scenery_ctx, &loft_ctx),
        ) else {
            continue;
        };
        references.push((open_at, reference));
    }

    // Запасной вариант для файлов без обёртки `iBlueprintLibrary-cAbsoluteBlueprintID`.
    let mut covered_index = 0usize;
    let mut cursor = 0usize;
    while let Some(position) = find_ignore_ascii_case(haystack, b"<Provider", cursor) {
        let Some((provider_range, product_range, blueprint_range, end)) =
            match_bare_blueprint(haystack, position)
        else {
            cursor = position + 1;
            continue;
        };
        cursor = end;
        update_context_state(
            xml,
            &mut context_cursor,
            position,
            &mut vehicle_ctx,
            &mut scenery_ctx,
            &mut loft_ctx,
        );
        while covered_index < covered.len() && covered[covered_index].end <= position {
            covered_index += 1;
        }
        if covered_index < covered.len() && covered[covered_index].contains(&position) {
            continue;
        }
        let Some(reference) = build_asset_reference(
            &xml[provider_range.clone()],
            &xml[product_range.clone()],
            &xml[blueprint_range.clone()],
            provider_range,
            product_range,
            blueprint_range,
            kind_from_context_state(&vehicle_ctx, &scenery_ctx, &loft_ctx),
        ) else {
            continue;
        };
        references.push((position, reference));
    }

    references.sort_by_key(|(position, _)| *position);
    references
        .into_iter()
        .map(|(_, reference)| reference)
        .collect()
}
/// Позиции значений «голой» ссылки: провайдер, продукт, блюпринт и конец.
type BlueprintRanges = (
    std::ops::Range<usize>,
    std::ops::Range<usize>,
    std::ops::Range<usize>,
    usize,
);
/// Разбирает «голую» ссылку `<Provider>…</Provider><Product>…</Product><BlueprintID>…</BlueprintID>`.
/// /// Между элементами допускается только текст: как и в исходном шаблоне
/// `[^<]*`, никакой другой тег между ними стоять не может.
pub(crate) fn match_bare_blueprint(haystack: &[u8], start: usize) -> Option<BlueprintRanges> {
    let (provider_range, after_provider) = parse_tagged_value(haystack, "Provider", start)?;
    let product_start = next_tag_start(haystack, after_provider)?;
    let (product_range, after_product) = parse_tagged_value(haystack, "Product", product_start)?;
    let blueprint_start = next_tag_start(haystack, after_product)?;
    let (blueprint_range, end) = parse_tagged_value(haystack, "BlueprintID", blueprint_start)?;
    Some((provider_range, product_range, blueprint_range, end))
}
#[cfg(test)]
pub(crate) fn extract_asset_references(xml: &str) -> std::collections::HashSet<String> {
    extract_asset_references_with_locations(xml)
        .into_iter()
        .map(|reference| reference.path)
        .collect()
}
/// Прежняя реализация разбора на регулярных выражениях.
/// /// Оставлена для сверки: ручной разбор обязан давать ровно тот же результат.
pub(crate) fn route_tile_label(
    route_path: &Path,
    source_file: &crate::domain::RouteFile,
) -> String {
    let relative = source_file
        .route_name(route_path)
        .unwrap_or_else(|| source_file.file_name());
    let file_name = source_file.file_name();
    let stem = Path::new(&file_name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default();
    let bytes = stem.as_bytes();
    if bytes.len() >= 14 {
        for start in 0..=bytes.len() - 14 {
            let tile = &bytes[start..start + 14];
            if (tile[0] == b'+' || tile[0] == b'-')
                && tile[1..7].iter().all(u8::is_ascii_digit)
                && (tile[7] == b'+' || tile[7] == b'-')
                && tile[8..14].iter().all(u8::is_ascii_digit)
            {
                return format!(
                    "тайл X={} Z={}",
                    String::from_utf8_lossy(&tile[..7]),
                    String::from_utf8_lossy(&tile[7..])
                );
            }
        }
    }
    relative
}

fn update_context_state(
    xml: &str,
    cursor: &mut usize,
    pos: usize,
    vehicle: &mut Option<usize>,
    scenery: &mut Option<usize>,
    loft: &mut Option<usize>,
) {
    if pos <= *cursor {
        return;
    }
    let end = pos.min(xml.len());
    let slice = &xml[*cursor..end];
    if let Some(index) = slice.rfind("<iRailVehicleBlueprint") {
        *vehicle = Some(*cursor + index);
    }
    if let Some(index) = slice.rfind("<SceneryRenderBlueprint") {
        *scenery = Some(*cursor + index);
    }
    if let Some(index) = slice.rfind("<RenderBlueprint") {
        *loft = Some(*cursor + index);
    }
    *cursor = end;
}

fn kind_from_context_state(
    vehicle: &Option<usize>,
    scenery: &Option<usize>,
    loft: &Option<usize>,
) -> crate::domain::AssetKind {
    let mut candidates: Vec<(usize, crate::domain::AssetKind)> = Vec::new();
    if let Some(index) = vehicle {
        candidates.push((*index, crate::domain::AssetKind::Vehicle));
    }
    if let Some(index) = scenery {
        candidates.push((*index, crate::domain::AssetKind::Scenery));
    }
    if let Some(index) = loft {
        candidates.push((*index, crate::domain::AssetKind::Loft));
    }
    candidates
        .into_iter()
        .max_by_key(|(index, _)| *index)
        .map(|(_, kind)| kind)
        .unwrap_or(crate::domain::AssetKind::Other)
}
