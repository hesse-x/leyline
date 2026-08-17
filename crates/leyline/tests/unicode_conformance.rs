use std::{fs, path::PathBuf};

use unicode_bidi::{BidiInfo, Level, UNICODE_VERSION};

#[test]
fn unicode_16_bidi_character_conformance() {
    assert_eq!(UNICODE_VERSION, (16, 0, 0));
    let path = fixture("BidiCharacterTest.txt");
    let source = fs::read_to_string(&path).expect("read locked Unicode bidi fixture");
    let mut cases = 0_usize;
    for (line_number, raw) in source.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        check_character_case(line)
            .unwrap_or_else(|error| panic!("{}:{}: {error}", path.display(), line_number + 1));
        cases += 1;
    }
    assert!(
        cases > 90_000,
        "fixture was unexpectedly filtered or truncated"
    );
}

#[test]
fn unicode_16_bidi_class_conformance() {
    let path = fixture("BidiTest.txt");
    let source = fs::read_to_string(&path).expect("read locked Unicode bidi fixture");
    let mut expected_levels = Vec::<String>::new();
    let mut expected_order = Vec::<usize>::new();
    let mut cases = 0_usize;
    for (line_number, raw) in source.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or_default().trim();
        if let Some(value) = line.strip_prefix("@Levels:") {
            expected_levels = value.split_whitespace().map(str::to_owned).collect();
            continue;
        }
        if let Some(value) = line.strip_prefix("@Reorder:") {
            expected_order = value
                .split_whitespace()
                .map(|item| item.parse().expect("fixture reorder index"))
                .collect();
            continue;
        }
        if line.is_empty() || line.starts_with('@') {
            continue;
        }
        let (classes, modes) = line
            .split_once(';')
            .unwrap_or_else(|| panic!("{}:{}: invalid data line", path.display(), line_number + 1));
        let text = classes
            .split_whitespace()
            .map(class_scalar)
            .collect::<Result<String, _>>()
            .unwrap_or_else(|error| panic!("{}:{}: {error}", path.display(), line_number + 1));
        let modes = u8::from_str_radix(modes.trim(), 16).expect("fixture paragraph bitset");
        for (bit, paragraph) in [(1, None), (2, Some(Level::ltr())), (4, Some(Level::rtl()))] {
            if modes & bit == 0 {
                continue;
            }
            check_resolved(&text, paragraph, &expected_levels, &expected_order).unwrap_or_else(
                |error| panic!("{}:{} mode {bit}: {error}", path.display(), line_number + 1),
            );
            cases += 1;
        }
    }
    assert!(
        cases > 750_000,
        "fixture was unexpectedly filtered or truncated"
    );
}

fn class_scalar(value: &str) -> Result<char, String> {
    match value {
        "L" => Ok('a'),
        "R" => Ok('\u{05d0}'),
        "AL" => Ok('\u{0627}'),
        "EN" => Ok('0'),
        "ES" => Ok('+'),
        "ET" => Ok('$'),
        "AN" => Ok('\u{0660}'),
        "CS" => Ok(','),
        "NSM" => Ok('\u{0300}'),
        "BN" => Ok('\u{00ad}'),
        "B" => Ok('\u{2029}'),
        "S" => Ok('\t'),
        "WS" => Ok(' '),
        "ON" => Ok('!'),
        "LRE" => Ok('\u{202a}'),
        "LRO" => Ok('\u{202d}'),
        "RLE" => Ok('\u{202b}'),
        "RLO" => Ok('\u{202e}'),
        "PDF" => Ok('\u{202c}'),
        "LRI" => Ok('\u{2066}'),
        "RLI" => Ok('\u{2067}'),
        "FSI" => Ok('\u{2068}'),
        "PDI" => Ok('\u{2069}'),
        _ => Err(format!("unknown bidi class {value}")),
    }
}

fn check_resolved(
    text: &str,
    paragraph: Option<Level>,
    expected_levels: &[String],
    expected_order: &[usize],
) -> Result<(), String> {
    let info = BidiInfo::new(text, paragraph);
    let para = info.paragraphs.first().ok_or("missing paragraph")?;
    let resolved = info.reordered_levels(para, para.range.clone());
    let starts = text
        .char_indices()
        .map(|(offset, _)| offset)
        .collect::<Vec<_>>();
    if starts.len() != expected_levels.len() {
        return Err("level count differs from scalar count".into());
    }
    let mut visible_levels = Vec::new();
    let mut visible_indices = Vec::new();
    for (index, (offset, expected)) in starts.iter().zip(expected_levels).enumerate() {
        if expected == "x" {
            continue;
        }
        let expected = expected
            .parse::<u8>()
            .map_err(|_| format!("invalid level {expected}"))?;
        let actual = resolved[*offset];
        if actual.number() != expected {
            return Err(format!(
                "level at scalar {index}: got {}, expected {expected}",
                actual.number()
            ));
        }
        visible_levels.push(actual);
        visible_indices.push(index);
    }
    let order = BidiInfo::reorder_visual(&visible_levels)
        .into_iter()
        .map(|visual| visible_indices[visual])
        .collect::<Vec<_>>();
    if order != expected_order {
        return Err(format!("order: got {order:?}, expected {expected_order:?}"));
    }
    Ok(())
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/unicode/16.0.0")
        .join(name)
}

fn check_character_case(line: &str) -> Result<(), String> {
    let fields = line.split(';').map(str::trim).collect::<Vec<_>>();
    if fields.len() != 5 {
        return Err(format!("expected five fields, got {}", fields.len()));
    }
    let chars = fields[0]
        .split_whitespace()
        .map(|value| {
            u32::from_str_radix(value, 16)
                .ok()
                .and_then(char::from_u32)
                .ok_or_else(|| format!("invalid scalar {value}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let text = chars.iter().collect::<String>();
    let paragraph = match fields[1] {
        "0" => Some(Level::ltr()),
        "1" => Some(Level::rtl()),
        "2" => None,
        value => return Err(format!("invalid paragraph mode {value}")),
    };
    let expected_paragraph = fields[2]
        .parse::<u8>()
        .map_err(|_| "invalid expected paragraph level".to_owned())?;
    let expected_levels = fields[3].split_whitespace().collect::<Vec<_>>();
    let expected_order = fields[4]
        .split_whitespace()
        .map(|value| {
            value
                .parse::<usize>()
                .map_err(|_| format!("invalid order {value}"))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let info = BidiInfo::new(&text, paragraph);
    let para = info.paragraphs.first().ok_or("missing paragraph")?;
    if para.level.number() != expected_paragraph {
        return Err(format!(
            "paragraph level: got {}, expected {expected_paragraph}",
            para.level.number()
        ));
    }
    let resolved = info.reordered_levels(para, para.range.clone());
    let starts = text
        .char_indices()
        .map(|(offset, _)| offset)
        .collect::<Vec<_>>();
    if starts.len() != expected_levels.len() {
        return Err("level count differs from scalar count".into());
    }
    let mut visible_levels = Vec::new();
    let mut visible_indices = Vec::new();
    for (index, (offset, expected)) in starts.iter().zip(&expected_levels).enumerate() {
        if *expected == "x" {
            continue;
        }
        let expected = expected
            .parse::<u8>()
            .map_err(|_| format!("invalid level {expected}"))?;
        let actual = resolved[*offset];
        if actual.number() != expected {
            return Err(format!(
                "level at scalar {index}: got {}, expected {expected}",
                actual.number()
            ));
        }
        visible_levels.push(actual);
        visible_indices.push(index);
    }
    let order = BidiInfo::reorder_visual(&visible_levels)
        .into_iter()
        .map(|visual| visible_indices[visual])
        .collect::<Vec<_>>();
    if order != expected_order {
        return Err(format!("order: got {order:?}, expected {expected_order:?}"));
    }
    Ok(())
}
