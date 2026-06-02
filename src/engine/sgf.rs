//! SGF → KataGo query data.
//!
//! KataGo's analysis engine takes a move list + metadata (not SGF), so we parse
//! the SGF with `sgf-parser` and emit GTP-style coordinates it understands.

use serde::{Deserialize, Serialize};
use sgf_parser::{Action, RuleSet, SgfToken};

use crate::error::{AppError, AppResult};

/// Stone colour. Serializes to KataGo's shorthand (`"B"` / `"W"`), which is also
/// how it appears in the analysis result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Color {
    /// Black.
    #[serde(rename = "B")]
    Black,
    /// White.
    #[serde(rename = "W")]
    White,
}

/// Scoring ruleset for KataGo. Serializes to KataGo's shorthand
/// (`"japanese"` / `"chinese"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Rules {
    /// Japanese (territory) scoring.
    Japanese,
    /// Chinese (area) scoring.
    Chinese,
}

/// A colour + location pair as KataGo expects it: `(Color::Black, "Q4")` /
/// `(Color::White, "pass")`.
pub type ColoredMove = (Color, String);

/// Parsed game ready to turn into a KataGo analysis query.
pub struct ParsedGame {
    /// Board size (square; KataGo gets this as both X and Y size).
    pub board_size: u8,
    /// Komi.
    pub komi: f32,
    /// Ruleset KataGo should score with.
    pub rules: Rules,
    /// Raw SGF result string, if present.
    pub result: Option<String>,
    /// Pre-placed stones (handicap / setup), as GTP coordinates.
    pub initial_stones: Vec<ColoredMove>,
    /// Moves in play order, as GTP coordinates.
    pub moves: Vec<ColoredMove>,
}

impl From<sgf_parser::Color> for Color {
    fn from(color: sgf_parser::Color) -> Self {
        match color {
            sgf_parser::Color::Black => Color::Black,
            sgf_parser::Color::White => Color::White,
        }
    }
}

/// Convert 1-indexed SGF coords (origin top-left) to a GTP string (column
/// letter skipping `I`, row counted from the bottom). `board_size` is needed to
/// flip the row to bottom-origin.
fn to_gtp(col: u8, line: u8, board_size: u8) -> String {
    const LETTERS: &[u8] = b"ABCDEFGHJKLMNOPQRSTUVWXYZ";
    let letter = *LETTERS.get((col - 1) as usize).unwrap_or(&b'?') as char;
    let row = board_size as u32 + 1 - line as u32;
    format!("{letter}{row}")
}

/// Parse an SGF into a [`ParsedGame`].
pub fn parse(sgf: &str, default_size: u8, default_komi: f32) -> AppResult<ParsedGame> {
    let tree = sgf_parser::parse(sgf).map_err(|err| AppError::Sgf(err.to_string()))?;

    let mut size = default_size;
    let mut komi: Option<f32> = None;
    let mut rules = Rules::Japanese;
    let mut result = None;
    let mut initial_stones = Vec::new();
    let mut moves = Vec::new();

    let mut first = true;
    for node in tree.iter() {
        if first {
            for token in &node.tokens {
                match token {
                    SgfToken::Komi(value) => komi = Some(*value),
                    // SGF size is (x, y); we assume square boards.
                    SgfToken::Size(x, _y) => size = *x as u8,
                    SgfToken::Result(outcome) => result = Some(format!("{outcome:?}")),
                    SgfToken::Rule(rule) => {
                        rules = match rule {
                            RuleSet::Chinese => Rules::Chinese,
                            // Japanese (and anything else KataGo doesn't model here).
                            _ => Rules::Japanese,
                        }
                    }
                    SgfToken::Add {
                        color,
                        coordinate: (x, y),
                    } => initial_stones.push(((*color).into(), to_gtp(*x, *y, size))),
                    _ => {}
                }
            }
            first = false;
        } else if let Some(SgfToken::Move { color, action }) = node
            .tokens
            .iter()
            .find(|token| matches!(token, SgfToken::Move { .. }))
        {
            let loc = match action {
                Action::Move(col, line) => to_gtp(*col, *line, size),
                Action::Pass => "pass".to_owned(),
            };
            moves.push(((*color).into(), loc));
        }
    }

    Ok(ParsedGame {
        board_size: size,
        komi: komi.unwrap_or(default_komi),
        rules,
        result,
        initial_stones,
        moves,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_metadata_and_gtp_moves() {
        let sgf = "(;GM[1]SZ[9]KM[6.5]RU[Japanese];B[ee];W[gg];B[cc])";
        let game = parse(sgf, 19, 7.5).unwrap();
        assert_eq!(game.board_size, 9);
        assert!((game.komi - 6.5).abs() < f32::EPSILON);
        assert_eq!(game.rules, Rules::Japanese);
        assert_eq!(game.moves.len(), 3);
        // ee on 9x9: col 5 -> 'E', line 5 -> row 9+1-5 = 5  => "E5", Black.
        assert_eq!(game.moves[0], (Color::Black, "E5".to_owned()));
        // gg: col 7 -> 'G', line 7 -> row 3 => "G3", White.
        assert_eq!(game.moves[1], (Color::White, "G3".to_owned()));
    }

    #[test]
    fn applies_defaults_when_size_and_komi_absent() {
        let sgf = "(;GM[1];B[pd];W[dp])";
        let game = parse(sgf, 19, 7.5).unwrap();
        assert_eq!(game.board_size, 19);
        assert!((game.komi - 7.5).abs() < f32::EPSILON);
        // pd on 19x19: col 16 -> 'Q', line 4 -> row 16 => "Q16".
        assert_eq!(game.moves[0], (Color::Black, "Q16".to_owned()));
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("not sgf", 19, 7.5).is_err());
    }
}
