//! Pure state and move validation for the two-player Chess game.
//!
//! `chess.js` is the Node authority today. Rust uses an equivalent legal-move generator rather
//! than accepting coordinates as if they were valid. Discord rendering, idle timers and scoring
//! remain adapter concerns.

use shakmaty::{Chess, Color, EnPassantMode, Move, Position, fen::Fen, san::San, uci::UciMove};
use std::num::NonZeroU32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChessColor {
    White,
    Black,
}

impl ChessColor {
    fn as_chess(self) -> Color {
        match self {
            Self::White => Color::White,
            Self::Black => Color::Black,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChessEvent {
    Ignored,
    Closed,
    Spectator,
    NotYourTurn {
        color: ChessColor,
    },
    IllegalMove {
        text: String,
    },
    Moved {
        user_id: String,
        name: String,
        text: String,
        next: ChessColor,
        in_check: bool,
    },
    Checkmate {
        winner_id: String,
        winner_name: String,
        text: String,
    },
    Draw {
        text: String,
    },
    Resigned {
        user_id: String,
        user_name: String,
        winner_id: String,
        winner_name: String,
    },
}

#[derive(Debug, Clone)]
pub struct ChessGame {
    game: Chess,
    white_id: Option<String>,
    black_id: Option<String>,
    white_name: Option<String>,
    black_name: Option<String>,
    over: bool,
}

impl Default for ChessGame {
    fn default() -> Self {
        Self::new()
    }
}

impl ChessGame {
    #[must_use]
    pub fn new() -> Self {
        Self {
            game: Chess::default(),
            white_id: None,
            black_id: None,
            white_name: None,
            black_name: None,
            over: false,
        }
    }

    #[must_use]
    pub fn is_over(&self) -> bool {
        self.over
    }

    #[must_use]
    pub fn white_id(&self) -> Option<&str> {
        self.white_id.as_deref()
    }

    #[must_use]
    pub fn black_id(&self) -> Option<&str> {
        self.black_id.as_deref()
    }

    #[must_use]
    pub fn white_name(&self) -> Option<&str> {
        self.white_name.as_deref()
    }

    #[must_use]
    pub fn black_name(&self) -> Option<&str> {
        self.black_name.as_deref()
    }

    #[must_use]
    pub fn turn(&self) -> ChessColor {
        chess_color(self.game.turn())
    }

    /// Current board position in standard FEN form for a transport/rendering adapter. Keeping
    /// this as a value snapshot avoids exposing the third-party chess board type across crates.
    #[must_use]
    pub fn board_fen(&self) -> String {
        // Preserve the existing board-only FEN snapshot counters.
        let mut setup = self.game.to_setup(EnPassantMode::Legal);
        setup.halfmoves = 0;
        setup.fullmoves = NonZeroU32::MIN;
        Fen::from_setup(setup).to_string()
    }

    /// Applies a move or resignation. The first two distinct users who submit a valid-looking
    /// move take White and Black, matching the Node game; later users become spectators.
    #[must_use]
    pub fn play(&mut self, user_id: &str, name: &str, raw: &str) -> ChessEvent {
        if self.over {
            return ChessEvent::Closed;
        }
        let text = raw.trim();
        if is_resignation(text) {
            return self.resign(user_id, name);
        }
        if !looks_like_move(text) {
            return ChessEvent::Ignored;
        }

        let Some(color) = self.seat(user_id, name) else {
            return ChessEvent::Spectator;
        };
        let turn = self.game.turn();
        if color.as_chess() != turn {
            return ChessEvent::NotYourTurn {
                color: chess_color(turn),
            };
        }
        let Some(chess_move) = parse_move(&self.game, text) else {
            return ChessEvent::IllegalMove {
                text: text.to_owned(),
            };
        };
        if !self.game.is_legal(chess_move) {
            return ChessEvent::IllegalMove {
                text: text.to_owned(),
            };
        }
        self.game.play_unchecked(chess_move);
        if self.game.is_checkmate() {
            self.over = true;
            let (winner_id, winner_name) = if color == ChessColor::White {
                (
                    self.white_id.clone().unwrap_or_default(),
                    self.white_name.clone().unwrap_or_default(),
                )
            } else {
                (
                    self.black_id.clone().unwrap_or_default(),
                    self.black_name.clone().unwrap_or_default(),
                )
            };
            ChessEvent::Checkmate {
                winner_id,
                winner_name,
                text: text.to_owned(),
            }
        } else if self.game.is_stalemate() {
            self.over = true;
            ChessEvent::Draw {
                text: text.to_owned(),
            }
        } else {
            ChessEvent::Moved {
                user_id: user_id.to_owned(),
                name: name.to_owned(),
                text: text.to_owned(),
                next: chess_color(self.game.turn()),
                in_check: self.game.is_check(),
            }
        }
    }

    fn seat(&mut self, user_id: &str, name: &str) -> Option<ChessColor> {
        if self.white_id.as_deref() == Some(user_id) {
            self.white_name = Some(name.to_owned());
            return Some(ChessColor::White);
        }
        if self.black_id.as_deref() == Some(user_id) {
            self.black_name = Some(name.to_owned());
            return Some(ChessColor::Black);
        }
        if self.white_id.is_none() {
            self.white_id = Some(user_id.to_owned());
            self.white_name = Some(name.to_owned());
            return Some(ChessColor::White);
        }
        if self.black_id.is_none() {
            self.black_id = Some(user_id.to_owned());
            self.black_name = Some(name.to_owned());
            return Some(ChessColor::Black);
        }
        None
    }

    fn resign(&mut self, user_id: &str, user_name: &str) -> ChessEvent {
        let Some(color) = self.color_of(user_id) else {
            return ChessEvent::Ignored;
        };
        let (Some(white), Some(black)) = (&self.white_id, &self.black_id) else {
            return ChessEvent::Ignored;
        };
        let winner_id = if color == ChessColor::White {
            black.clone()
        } else {
            white.clone()
        };
        let winner_name = if color == ChessColor::White {
            self.black_name.clone().unwrap_or_default()
        } else {
            self.white_name.clone().unwrap_or_default()
        };
        if self.game.is_checkmate() || self.game.is_stalemate() {
            return ChessEvent::Ignored;
        }
        self.over = true;
        ChessEvent::Resigned {
            user_id: user_id.to_owned(),
            user_name: user_name.to_owned(),
            winner_id,
            winner_name,
        }
    }

    fn color_of(&self, user_id: &str) -> Option<ChessColor> {
        if self.white_id.as_deref() == Some(user_id) {
            Some(ChessColor::White)
        } else if self.black_id.as_deref() == Some(user_id) {
            Some(ChessColor::Black)
        } else {
            None
        }
    }
}

fn chess_color(color: Color) -> ChessColor {
    match color {
        Color::White => ChessColor::White,
        Color::Black => ChessColor::Black,
    }
}

fn is_resignation(text: &str) -> bool {
    matches!(
        text.to_ascii_lowercase().as_str(),
        "resign" | "resigns" | "i resign" | "desisto" | "desistir"
    )
}

fn looks_like_move(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    if (4..=5).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && (b'1'..=b'8').contains(&bytes[1])
        && bytes[2].is_ascii_lowercase()
        && (b'1'..=b'8').contains(&bytes[3])
    {
        return bytes.len() == 4 || matches!(bytes[4], b'q' | b'r' | b'b' | b'n');
    }
    let normalized = lower.strip_suffix(['+', '#']).unwrap_or(&lower);
    let castle = matches!(normalized, "o-o" | "o-o-o");
    let destination = normalized
        .as_bytes()
        .windows(2)
        .any(|pair| (b'a'..=b'h').contains(&pair[0]) && (b'1'..=b'8').contains(&pair[1]));
    castle || destination
}

fn parse_move(board: &Chess, text: &str) -> Option<Move> {
    let lower = text.to_ascii_lowercase();
    let coordinate = lower.as_bytes();
    if (4..=5).contains(&coordinate.len())
        && coordinate[0].is_ascii_lowercase()
        && (b'1'..=b'8').contains(&coordinate[1])
        && coordinate[2].is_ascii_lowercase()
        && (b'1'..=b'8').contains(&coordinate[3])
    {
        return UciMove::from_ascii(coordinate).ok()?.to_move(board).ok();
    }
    let san = if lower == "o-o" || lower == "o-o-o" {
        text.to_ascii_uppercase()
    } else {
        text.to_owned()
    };
    // Preserve the legacy promotion and explicit en-passant SAN spellings.
    let explicit_en_passant = san.ends_with(" e.p.");
    let san = san.strip_suffix(" e.p.").unwrap_or(&san);
    let core = san.strip_suffix(['+', '#']).unwrap_or(san);
    let mut normalized = san.to_owned();
    let bytes = core.as_bytes();
    if bytes.len() >= 2
        && matches!(bytes[bytes.len() - 1], b'Q' | b'R' | b'B' | b'N')
        && matches!(bytes[bytes.len() - 2], b'1' | b'8')
    {
        normalized.insert(core.len() - 1, '=');
    }
    let chess_move = San::from_ascii(normalized.as_bytes())
        .ok()?
        .to_move(board)
        .ok()?;
    if explicit_en_passant && !matches!(chess_move, Move::EnPassant { .. }) {
        return None;
    }
    Some(chess_move)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_two_players_are_seated_and_turns_are_enforced() {
        let mut game = ChessGame::new();
        assert!(matches!(
            game.play("w", "White", "e2e4"),
            ChessEvent::Moved {
                next: ChessColor::Black,
                ..
            }
        ));
        assert!(matches!(
            game.play("w", "White", "d2d4"),
            ChessEvent::NotYourTurn {
                color: ChessColor::Black
            }
        ));
        assert_eq!(game.white_id(), Some("w"));
        assert!(matches!(
            game.play("b", "Black", "e7e5"),
            ChessEvent::Moved {
                next: ChessColor::White,
                ..
            }
        ));
    }

    #[test]
    fn illegal_move_still_claims_a_seat_but_chat_does_not() {
        let mut game = ChessGame::new();
        assert!(matches!(
            game.play("w", "White", "e2e5"),
            ChessEvent::IllegalMove { .. }
        ));
        assert_eq!(game.white_id(), Some("w"));
        assert!(matches!(
            game.play("chat", "Chat", "hello"),
            ChessEvent::Ignored
        ));
        assert_eq!(game.black_id(), None);
        assert!(matches!(
            game.play("b", "Black", "e7e5"),
            ChessEvent::NotYourTurn {
                color: ChessColor::White
            }
        ));
        assert_eq!(game.black_id(), Some("b"));
    }

    #[test]
    fn illegal_move_does_not_change_board_and_resignation_awards_opponent() {
        let mut game = ChessGame::new();
        assert!(matches!(
            game.play("w", "White", "e2e4"),
            ChessEvent::Moved { .. }
        ));
        assert!(matches!(
            game.play("b", "Black", "e7e5"),
            ChessEvent::Moved { .. }
        ));
        assert!(matches!(
            game.play("w", "White", "e2e5"),
            ChessEvent::IllegalMove { .. }
        ));
        assert!(matches!(
            game.play("w", "White", "resign"),
            ChessEvent::Resigned { winner_id, .. } if winner_id == "b"
        ));
        assert!(game.is_over());
    }

    #[test]
    fn checkmate_is_detected_by_legal_move_generator() {
        let mut game = ChessGame::new();
        for (user, name, move_text) in [
            ("w", "White", "f2f3"),
            ("b", "Black", "e7e5"),
            ("w", "White", "g2g4"),
            ("b", "Black", "d8h4"),
        ] {
            let event = game.play(user, name, move_text);
            if move_text == "d8h4" {
                assert!(matches!(event, ChessEvent::Checkmate { .. }));
            }
        }
        assert!(game.is_over());
    }

    #[test]
    fn check_is_reported_before_checkmate() {
        let mut game = ChessGame::new();
        for (user, name, move_text) in [
            ("w", "White", "e2e4"),
            ("b", "Black", "e7e5"),
            ("w", "White", "d1h5"),
            ("b", "Black", "g7g6"),
            ("w", "White", "h5e5"),
        ] {
            let event = game.play(user, name, move_text);
            if move_text == "h5e5" {
                assert!(matches!(
                    event,
                    ChessEvent::Moved {
                        in_check: true,
                        next: ChessColor::Black,
                        ..
                    }
                ));
            }
        }
    }

    #[test]
    fn exposes_a_stable_fen_snapshot_without_leaking_the_board_type() {
        let game = ChessGame::new();
        assert!(
            game.board_fen()
                .starts_with("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w")
        );
    }
    #[test]
    fn san_castling_and_board_only_fen_contract_are_preserved() {
        let mut game = ChessGame::new();
        for (index, text) in ["e4", "e5", "Nf3", "Nc6", "Bc4", "Nf6", "o-o"]
            .into_iter()
            .enumerate()
        {
            let user = if index % 2 == 0 { "w" } else { "b" };
            assert!(
                matches!(game.play(user, user, text), ChessEvent::Moved { .. }),
                "{text}"
            );
        }
        assert_eq!(
            game.board_fen(),
            "r1bqkb1r/pppp1ppp/2n2n2/4p3/2B1P3/5N2/PPPP1PPP/RNBQ1RK1 b kq - 0 1"
        );
    }

    #[test]
    fn en_passant_and_uppercase_coordinates_keep_standard_legality() {
        let mut game = ChessGame::new();
        for (index, text) in ["E2E4", "a6", "e5", "f5", "exf6"].into_iter().enumerate() {
            let user = if index % 2 == 0 { "w" } else { "b" };
            assert!(
                matches!(game.play(user, user, text), ChessEvent::Moved { .. }),
                "{text}"
            );
        }
        assert!(
            game.board_fen()
                .starts_with("rnbqkbnr/1pppp1pp/p4P2/8/8/8/PPPP1PPP/RNBQKBNR b")
        );
    }

    fn position(fen: &str) -> ChessGame {
        ChessGame {
            game: Fen::from_ascii(fen.as_bytes())
                .unwrap()
                .into_position(shakmaty::CastlingMode::Standard)
                .unwrap(),
            ..ChessGame::new()
        }
    }

    #[test]
    fn all_promotions_are_legal_but_missing_promotion_is_rejected() {
        for (suffix, piece) in [('q', 'Q'), ('r', 'R'), ('b', 'B'), ('n', 'N')] {
            let mut game = position("7k/P7/8/8/8/8/8/K7 w - - 0 1");
            let text = format!("a7a8{suffix}");
            assert!(matches!(
                game.play("w", "White", &text),
                ChessEvent::Moved { .. }
            ));
            assert!(game.board_fen().starts_with(&format!("{piece}6k/")));
        }
        let mut game = position("7k/P7/8/8/8/8/8/K7 w - - 0 1");
        assert!(matches!(
            game.play("w", "White", "a7a8"),
            ChessEvent::IllegalMove { .. }
        ));
    }

    #[test]
    fn legacy_san_promotion_and_en_passant_aliases_remain_legal() {
        for piece in ['Q', 'R', 'B', 'N'] {
            for text in [format!("a8{piece}"), format!("a8={piece}")] {
                let mut game = position("7k/P7/8/8/8/8/8/K7 w - - 0 1");
                assert!(
                    matches!(game.play("w", "White", &text), ChessEvent::Moved { .. }),
                    "{text}"
                );
                assert!(game.board_fen().starts_with(&format!("{piece}6k/")));
            }
        }
        let mut game = ChessGame::new();
        for (index, text) in ["e4", "a6", "e5", "f5", "exf6 e.p."]
            .into_iter()
            .enumerate()
        {
            let user = if index % 2 == 0 { "w" } else { "b" };
            assert!(
                matches!(game.play(user, user, text), ChessEvent::Moved { .. }),
                "{text}"
            );
        }
        assert!(game.board_fen().starts_with("rnbqkbnr/1pppp1pp/p4P2/"));
        let mut game = ChessGame::new();
        assert!(matches!(
            game.play("w", "White", "e4 e.p."),
            ChessEvent::IllegalMove { .. }
        ));
    }

    #[test]
    fn pinned_moves_and_castling_through_check_do_not_change_position() {
        for (fen, text) in [
            ("4r2k/8/8/8/8/8/4R3/4K3 w - - 0 1", "e2f2"),
            ("k4r2/8/8/8/8/8/8/4K2R w K - 0 1", "O-O"),
        ] {
            let mut game = position(fen);
            let before = game.board_fen();
            assert!(matches!(
                game.play("w", "White", text),
                ChessEvent::IllegalMove { .. }
            ));
            assert_eq!(game.board_fen(), before);
            assert_eq!(game.turn(), ChessColor::White);
        }
    }

    #[test]
    fn stalemate_and_san_checkmate_keep_terminal_event_contracts() {
        let mut stalemate = position("7k/5Q2/6K1/8/8/8/8/8 w - - 0 1");
        assert!(matches!(
            stalemate.play("w", "White", "Qe6"),
            ChessEvent::Draw { .. }
        ));
        assert!(stalemate.is_over());
        let mut mate = ChessGame::new();
        for (index, text) in ["f3", "e5", "g4", "Qh4#"].into_iter().enumerate() {
            let user = if index % 2 == 0 { "w" } else { "b" };
            let event = mate.play(user, user, text);
            if index == 3 {
                assert!(
                    matches!(event, ChessEvent::Checkmate { winner_id, text, .. } if winner_id == "b" && text == "Qh4#")
                );
            }
        }
        assert_eq!(mate.play("w", "White", "e4"), ChessEvent::Closed);
    }

    #[test]
    fn engine_replacement_does_not_introduce_automatic_draws() {
        let mut game = ChessGame::new();
        for _ in 0..3 {
            for (index, text) in ["Nf3", "Nf6", "Ng1", "Ng8"].into_iter().enumerate() {
                let user = if index % 2 == 0 { "w" } else { "b" };
                assert!(matches!(
                    game.play(user, user, text),
                    ChessEvent::Moved { .. }
                ));
            }
        }
        assert!(!game.is_over());
        let mut bare_kings = position("7k/8/6K1/8/8/8/8/8 w - - 0 1");
        assert!(matches!(
            bare_kings.play("w", "White", "g6f6"),
            ChessEvent::Moved { .. }
        ));
        let mut move_count = position("7k/8/8/8/8/8/8/KN6 w - - 99 50");
        assert!(matches!(
            move_count.play("w", "White", "b1c3"),
            ChessEvent::Moved { .. }
        ));
    }

    #[test]
    fn resignation_aliases_work_out_of_turn_and_preserve_player_contracts() {
        for alias in ["resign", "RESIGNS", " i resign ", "desisto", "desistir"] {
            let mut game = ChessGame::new();
            assert_eq!(game.play("spectator", "Other", alias), ChessEvent::Ignored);
            let _ = game.play("w", "White", "e4");
            assert_eq!(game.play("w", "White", alias), ChessEvent::Ignored);
            let _ = game.play("b", "Black", "e5");
            assert!(
                matches!(game.play("b", "Updated Black", alias), ChessEvent::Resigned { winner_id, user_name, .. } if winner_id == "w" && user_name == "Updated Black")
            );
            assert_eq!(game.play("w", "White", "e4"), ChessEvent::Closed);
        }
    }
}
