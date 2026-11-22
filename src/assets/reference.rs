//! Разбор ссылок на ассеты внутри XML RailWorks.
//!
//! Ссылка выглядит так: `<Provider>/<Product>/<путь>`. В маршрутах она может
//! быть без расширения и с суффиксом варианта `[00]`, а в файлах сценарных
//! поездов — с указанием на объект поезда.

use std::ops::Range;
use std::path::{Component, Path};

use crate::common::domain::{AssetKind, RouteFile};
use crate::common::translit::decode_xml_text;

pub(crate) struct AssetReference {
    pub(crate) path: String,
    pub(crate) provider_range: Range<usize>,
    pub(crate) product_range: Range<usize>,
    pub(crate) blueprint_range: Range<usize>,
    /// Что за объект стоит по ссылке: объект маршрута или файл состава.
    pub(crate) kind: AssetKind,
}

/// Позиции значений «голой» ссылки: провайдер, продукт, блюпринт и конец.
type BlueprintRanges = (Range<usize>, Range<usize>, Range<usize>, usize);

/// Возвращает байтовый диапазон значения совпадения без окружающих пробелов.
pub(crate) fn capture_value_range(capture: &regex::Match<'_>) -> Range<usize> {
    let value = capture.as_str();
    let leading = value.len() - value.trim_start().len();
    let trimmed = value.trim();
    capture.start() + leading..capture.start() + leading + trimmed.len()
}

/// Ищет первое вхождение подстроки без учёта регистра ASCII.
///
/// Нужен для ручного разбора XML: регулярное выражение с ленивым `.*?` заново
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
            .position(|byte| byte.eq_ignore_ascii_case(&first))?;
        let start = index + offset;
        if haystack[start..start + needle.len()].eq_ignore_ascii_case(needle) {
            return Some(start);
        }
        index = start + 1;
    }
    None
}

/// Ищет элемент `<Тег ...>значение</Тег>` начиная с заданного места.
///
/// Возвращает диапазон значения без окружающих пробелов и позицию сразу за
/// закрывающим тегом. Значение не может содержать `<`, как и в исходном
/// шаблоне `([^<]*)`.
pub(crate) fn parse_tagged_value(
    haystack: &[u8],
    tag: &str,
    start: usize,
) -> Option<(Range<usize>, usize)> {
    let tag_bytes = tag.as_bytes();
    let after_open = start.checked_add(1)?;
    if *haystack.get(start)? != b'<' {
        return None;
    }
    let tag_start = after_open.checked_add(tag_bytes.len())?;
    if !haystack
        .get(after_open..tag_start)?
        .eq_ignore_ascii_case(tag_bytes)
    {
        return None;
    }
    let next_byte = *haystack.get(tag_start)?;
    if next_byte != b'>' && !next_byte.is_ascii_whitespace() && next_byte != b'/' {
        return None;
    }
    let tag_end = tag_start
        + haystack
            .get(tag_start..)?
            .iter()
            .position(|&byte| byte == b'>')?;
    if haystack[tag_end - 1] == b'/' {
        return None;
    }
    let value_start = tag_end + 1;
    let value_end = value_start
        + haystack
            .get(value_start..)?
            .iter()
            .position(|&byte| byte == b'<')?;
    if *haystack.get(value_end)? != b'<' || *haystack.get(value_end + 1)? != b'/' {
        return None;
    }
    let close_tag_start = value_end + 2;
    let close_end = close_tag_start.checked_add(tag_bytes.len())?;
    if !haystack
        .get(close_tag_start..close_end)?
        .eq_ignore_ascii_case(tag_bytes)
    {
        return None;
    }
    if *haystack.get(close_end)? != b'>' {
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
        .map_or(leading, |last| last + 1);
    Some((value_start + leading..value_start + trimmed, close_end + 1))
}

/// Позиция следующего открывающего тега после заданного места.
pub(crate) fn next_tag_start(haystack: &[u8], from: usize) -> Option<usize> {
    haystack
        .get(from..)?
        .iter()
        .position(|&byte| byte == b'<')
        .map(|offset| from + offset)
}

/// Ищет элемент по имени в заданном диапазоне файла.
pub(crate) fn find_tagged_value(
    haystack: &[u8],
    tag: &str,
    from: usize,
    limit: usize,
) -> Option<(Range<usize>, usize)> {
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
    const OPEN: &[u8] = b"<iBlueprintLibrary-cAbsoluteBlueprintID>";
    const CLOSE: &[u8] = b"</iBlueprintLibrary-cAbsoluteBlueprintID>";
    let mut blocks = Vec::new();
    let mut cursor = 0usize;
    while let Some(open_at) = find_ignore_ascii_case(haystack, OPEN, cursor) {
        let content_start = open_at + OPEN.len();
        let Some(close_at) = find_ignore_ascii_case(haystack, CLOSE, content_start) else {
            break;
        };
        let block_end = close_at + CLOSE.len();
        blocks.push((open_at, block_end, content_start, close_at));
        cursor = block_end;
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
    let mut has_components = false;
    for component in relative_path.components() {
        if !matches!(component, Component::Normal(_)) {
            return None;
        }
        has_components = true;
    }
    if !has_components {
        return None;
    }
    Some(relative_path.to_string_lossy().replace('\\', "/"))
}

pub(crate) fn build_asset_reference(
    provider: &str,
    product: &str,
    blueprint: &str,
    provider_range: Range<usize>,
    product_range: Range<usize>,
    blueprint_range: Range<usize>,
    kind: AssetKind,
) -> Option<AssetReference> {
    let provider = decode_xml_text(provider.trim());
    let product = decode_xml_text(product.trim());
    let blueprint = decode_xml_text(blueprint.trim());
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
///
/// Основной источник — блоки `iBlueprintLibrary-cAbsoluteBlueprintID`, внутри которых
/// Provider, Product и путь к блюпринту принадлежат одной ссылке. Такой разбор не даёт
/// пустым заготовкам `ReskinBlueprintID` «съесть» ссылку следующего за ними объекта.
///
/// Разбор идёт вручную, без регулярных выражений с ленивым `.*?`: такие шаблоны
/// просматривают остаток файла заново для каждого блока и делают проверку маршрута
/// квадратичной по размеру XML.
pub(crate) fn extract_asset_references_with_locations(xml: &str) -> Vec<AssetReference> {
    let haystack = xml.as_bytes();
    let blocks = absolute_blueprint_blocks(haystack);
    let mut references: Vec<(usize, AssetReference)> = Vec::with_capacity(blocks.len());
    let mut covered: Vec<Range<usize>> = Vec::with_capacity(blocks.len());

    // Контекст (scenery/loft/vehicle) определяется ближайшим предыдущим тегом.
    // Чтобы не делать O(N) rfind для каждого блока на гигантском Tracks.bin,
    // поддерживаем индексы последних тегов инкрементально.
    let mut context_cursor = 0usize;
    let mut vehicle_ctx: Option<usize> = None;
    let mut scenery_ctx: Option<usize> = None;
    let mut loft_ctx: Option<usize> = None;

    for (open_at, block_end, content_start, content_end) in blocks {
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
            kind_from_context_state(vehicle_ctx, scenery_ctx, loft_ctx),
        ) else {
            continue;
        };
        references.push((open_at, reference));
    }

    // Запасной вариант для файлов без обёртки `iBlueprintLibrary-cAbsoluteBlueprintID`.
    // Сбрасываем состояние контекста, так как второй проход идёт с начала файла.
    context_cursor = 0;
    vehicle_ctx = None;
    scenery_ctx = None;
    loft_ctx = None;

    let mut covered_index = 0usize;
    let mut cursor = 0usize;
    let mut needs_sort = false;
    while let Some(position) = find_ignore_ascii_case(haystack, b"<Provider", cursor) {
        while covered_index < covered.len() && covered[covered_index].end <= position {
            covered_index += 1;
        }
        if covered_index < covered.len() && covered[covered_index].contains(&position) {
            cursor = covered[covered_index].end;
            continue;
        }
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
        let Some(reference) = build_asset_reference(
            &xml[provider_range.clone()],
            &xml[product_range.clone()],
            &xml[blueprint_range.clone()],
            provider_range,
            product_range,
            blueprint_range,
            kind_from_context_state(vehicle_ctx, scenery_ctx, loft_ctx),
        ) else {
            continue;
        };
        references.push((position, reference));
        needs_sort = true;
    }

    if needs_sort {
        references.sort_by_key(|(position, _)| *position);
    }
    references
        .into_iter()
        .map(|(_, reference)| reference)
        .collect()
}

/// Разбирает «голую» ссылку `<Provider>…</Provider><Product>…</Product><BlueprintID>…</BlueprintID>`.
/// Между элементами допускается только текст: как и в исходном шаблоне
/// `[^<]*`, никакой другой тег между ними стоять не может.
pub(crate) fn match_bare_blueprint(haystack: &[u8], start: usize) -> Option<BlueprintRanges> {
    let (provider_range, after_provider) = parse_tagged_value(haystack, "Provider", start)?;
    let product_start = next_tag_start(haystack, after_provider)?;
    let (product_range, after_product) = parse_tagged_value(haystack, "Product", product_start)?;
    let blueprint_start = next_tag_start(haystack, after_product)?;
    let (blueprint_range, end) = parse_tagged_value(haystack, "BlueprintID", blueprint_start)?;
    Some((provider_range, product_range, blueprint_range, end))
}

/// Извлекает метку тайла вида `+000001-000002` из имени файла маршрута
/// или возвращает имя файла/маршрута, если координаты не найдены.
pub(crate) fn route_tile_label(route_path: &Path, source_file: &RouteFile) -> String {
    let file_name = source_file.file_name_str();
    let stem = Path::new(file_name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default();
    let bytes = stem.as_bytes();
    if let Some(tile) = bytes.windows(14).find(|tile| {
        matches!(tile[0], b'+' | b'-')
            && tile[1..7].iter().all(u8::is_ascii_digit)
            && matches!(tile[7], b'+' | b'-')
            && tile[8..14].iter().all(u8::is_ascii_digit)
    }) {
        // Все 14 байт гарантированно валидный ASCII.
        return String::from_utf8(tile.to_vec()).unwrap_or_default();
    }
    source_file
        .route_name(route_path)
        .unwrap_or_else(|| source_file.file_name())
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
    vehicle: Option<usize>,
    scenery: Option<usize>,
    loft: Option<usize>,
) -> AssetKind {
    let mut best: Option<(usize, AssetKind)> = None;
    if let Some(index) = vehicle {
        best = Some((index, AssetKind::Vehicle));
    }
    if let Some(index) = scenery
        && best.is_none_or(|(current, _)| index > current)
    {
        best = Some((index, AssetKind::Scenery));
    }
    if let Some(index) = loft
        && best.is_none_or(|(current, _)| index > current)
    {
        best = Some((index, AssetKind::Loft));
    }
    best.map_or(AssetKind::Other, |(_, kind)| kind)
}
