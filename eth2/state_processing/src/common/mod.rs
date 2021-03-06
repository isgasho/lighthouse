mod deposit_data_tree;
mod get_attesting_indices;
mod get_base_reward;
mod get_indexed_attestation;
mod initiate_validator_exit;
mod slash_validator;

pub use deposit_data_tree::DepositDataTree;
pub use get_attesting_indices::get_attesting_indices;
pub use get_base_reward::get_base_reward;
pub use get_indexed_attestation::get_indexed_attestation;
pub use initiate_validator_exit::initiate_validator_exit;
pub use slash_validator::slash_validator;
