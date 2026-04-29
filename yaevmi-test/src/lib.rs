// git clone --depth 1 https://github.com/ethereum/tests yaevmi-test/tests 2>&1
// cd yaevmi-test/tests && tar -xzf fixtures_general_state_tests.tgz && rm -rf .git
// cargo test -p yaevmi-test --release --ignored --tests eth -- --ignored

#[cfg(test)]
pub mod eth;

pub mod revm;

#[cfg(test)]
pub mod sol;

#[cfg(test)]
pub mod exe;
