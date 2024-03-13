use alloy_primitives::{Address as AlloyAddress, Signature, U256};
use chess::{Board, BoardStatus, ChessMove};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;

type GameId = U256;

#[derive(Serialize, Deserialize)]
pub struct Game {
    turns: u64,
    board: String,
    white: AlloyAddress,
    black: AlloyAddress,
    wager: U256,
}

#[derive(Serialize, Deserialize)]
pub struct PendingGame {
    white: AlloyAddress,
    black: AlloyAddress,
    accepted: (bool, bool),
    wager: U256,
}

#[derive(Serialize, Deserialize)]
pub struct RollupState {
    pub sequenced: Vec<WrappedTransaction>,
    pub balances: HashMap<AlloyAddress, U256>,
    pub withdrawals: Vec<(AlloyAddress, U256)>,
    pub pending_games: HashMap<GameId, PendingGame>,
    pub games: HashMap<GameId, Game>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WrappedTransaction {
    pub pub_key: AlloyAddress,
    pub sig: Signature,
    pub data: TxType,
    // NOTE: I realized that TxType has to have a deterministic way to (de)serialize itself
    //   otherwise you'll pull your hair out wondering why the sig verification isn't working...
    //   perhaps EIP712 fixes this? Need to look. Regardless, just noting for now - fix later...
    // TODO probably need to add nonces, value, gas, gasPrice, gasLimit, ... but whatever
    // I think we could use eth_sendRawTransaction to just send arbitrary bytes to a sequencer
    // or at the very least we can use eth_signMessage plus an http request to this process
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum TxType {
    BridgeTokens(U256),
    WithdrawTokens(U256),
    Transfer {
        from: AlloyAddress,
        to: AlloyAddress,
        amount: U256,
    },
    ProposeGame {
        white: AlloyAddress,
        black: AlloyAddress,
        wager: U256,
    },
    StartGame(U256),
    Move {
        game_id: U256,
        san: String,
    },
    ClaimWin(U256),
}

pub fn chain_event_loop(tx: WrappedTransaction, state: &mut RollupState) -> anyhow::Result<()> {
    let decode_tx = tx.clone();

    if decode_tx
        .sig
        .recover_address_from_msg(&serde_json::to_string(&decode_tx.data).unwrap().as_bytes())
        .unwrap()
        != decode_tx.pub_key
    {
        return Err(anyhow::anyhow!("bad sig"));
    }

    match decode_tx.data {
        TxType::BridgeTokens(_) => Ok(()),
        TxType::WithdrawTokens(amount) => {
            state.balances.insert(
                tx.pub_key.clone(),
                state.balances.get(&tx.pub_key).unwrap_or(&U256::ZERO) - amount,
            );
            state.withdrawals.push((tx.pub_key, amount));
            state.sequenced.push(tx);
            Ok(())
        }
        TxType::Transfer { from, to, amount } => {
            state.balances.insert(
                from.clone(),
                state.balances.get(&from).unwrap_or(&U256::ZERO) - amount,
            );
            state.balances.insert(
                to.clone(),
                state.balances.get(&to).unwrap_or(&U256::ZERO) + amount,
            );
            state.sequenced.push(tx);
            Ok(())
        }
        TxType::ProposeGame {
            white,
            black,
            wager,
        } => {
            let game_id = U256::from(state.pending_games.len());
            state.pending_games.insert(
                game_id,
                PendingGame {
                    white: white.clone(),
                    black: black.clone(),
                    accepted: if white == tx.pub_key {
                        (true, false)
                    } else if black == tx.pub_key {
                        (false, true)
                    } else {
                        return Err(anyhow::anyhow!("not a player"));
                    },
                    wager,
                },
            );
            state.sequenced.push(tx);
            Ok(())
        }
        TxType::StartGame(game_id) => {
            let Some(pending_game) = state.pending_games.get(&game_id) else {
                return Err(anyhow::anyhow!("game id doesn't exist"));
            };
            if pending_game.accepted == (true, false) {
                if tx.pub_key != pending_game.black {
                    return Err(anyhow::anyhow!("not white"));
                }
            } else if pending_game.accepted == (false, true) {
                if tx.pub_key != pending_game.white {
                    return Err(anyhow::anyhow!("not black"));
                }
            } else {
                return Err(anyhow::anyhow!("impossible to reach"));
            }

            let Some(white_balance) = state.balances.get(&pending_game.white) else {
                return Err(anyhow::anyhow!("white doesn't exist"));
            };
            let Some(black_balance) = state.balances.get(&pending_game.black) else {
                return Err(anyhow::anyhow!("black doesn't exist"));
            };

            if white_balance < &pending_game.wager || black_balance < &pending_game.wager {
                return Err(anyhow::anyhow!("insufficient funds"));
            }

            state.balances.insert(
                pending_game.white.clone(),
                state.balances.get(&pending_game.white).unwrap() - pending_game.wager,
            );
            state.balances.insert(
                pending_game.black.clone(),
                state.balances.get(&pending_game.black).unwrap() - pending_game.wager,
            );

            state.games.insert(
                game_id,
                Game {
                    turns: 0,
                    board: Board::default().to_string(),
                    white: pending_game.white.clone(),
                    black: pending_game.black.clone(),
                    wager: pending_game.wager * U256::from(2),
                },
            );
            state.pending_games.remove(&game_id);
            state.sequenced.push(tx);
            Ok(())
        }
        TxType::Move { game_id, san } => {
            let Some(game) = state.games.get_mut(&game_id) else {
                return Err(anyhow::anyhow!("game id doesn't exist"));
            };

            if game.turns % 2 == 0 && tx.pub_key != game.white {
                return Err(anyhow::anyhow!("not white's turn"));
            } else if game.turns % 2 == 1 && tx.pub_key != game.black {
                return Err(anyhow::anyhow!("not black's turn"));
            }

            let mut board = Board::from_str(&game.board).unwrap();
            board = board.make_move_new(ChessMove::from_san(&board, &san).expect("invalid move"));
            game.board = board.to_string();
            game.turns += 1;
            state.sequenced.push(tx);
            Ok(())
        }
        TxType::ClaimWin(game_id) => {
            let game = state
                .games
                .get_mut(&game_id)
                .expect("game id doesn't exist");
            let board = Board::from_str(&game.board).unwrap();

            if board.status() == BoardStatus::Checkmate {
                if game.turns % 2 == 0 {
                    state.balances.insert(
                        game.black.clone(),
                        state.balances.get(&game.black).unwrap() + game.wager,
                    );
                } else {
                    state.balances.insert(
                        game.white.clone(),
                        state.balances.get(&game.white).unwrap() + game.wager,
                    );
                }
            } else if board.status() == BoardStatus::Stalemate {
                state.balances.insert(
                    game.white.clone(),
                    state.balances.get(&game.white).unwrap() + game.wager / U256::from(2),
                );
                state.balances.insert(
                    game.black.clone(),
                    state.balances.get(&game.black).unwrap() + game.wager / U256::from(2),
                );
            } else {
                return Err(anyhow::anyhow!("game is not over"));
            }

            state.games.remove(&game_id);
            Ok(())
        }
    }
}
