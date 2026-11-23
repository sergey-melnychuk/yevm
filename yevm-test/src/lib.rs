// git clone --depth 1 https://github.com/ethereum/tests yevm-test/tests 2>&1
// cd yevm-test/tests && tar -xzf fixtures_general_state_tests.tgz && rm -rf .git
// cargo test -p yevm-test --release --tests eth -- --ignored

#[cfg(test)]
pub mod eth;

#[cfg(test)]
pub mod eest;

pub mod revm;

#[cfg(test)]
pub mod sol;

#[cfg(test)]
pub mod exe;
