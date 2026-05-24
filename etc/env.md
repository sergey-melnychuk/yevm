Running benchmark on DigitalOcean droplet:

1. Droplet setup

```
BOX=

ssh -i ~/.ssh/ext root@$BOX
wget https://gist.github.com/sergey-melnychuk/6ed37068e946c7937669ab464822a8ae/raw/a307237fd979fdda88ae144e40f8a6b2c945d063/setup.sh
chmod +x setup.sh
./setup.sh
^D
```

2. Benchmark

```
ssh -i ~/.ssh/ext ext@$BOX

sudo apt update
sudo apt upgrade -y
sudo apt install -y build-essential libssl-dev pkg-config

curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
. "$HOME/.cargo/env"

git clone https://github.com/sergey-melnychuk/yevm.git -b wtf
cd yevm

export URL=
echo "YEVM_RPC_URL=$URL" > .env

cargo build --release --bin replay
cargo build --release --bin bench

./target/release/replay 24929490
./target/release/replay 24929491

./target/release/bench 24929490 10
./target/release/bench 24929491 10
```

3. Results

* MBP: MacBook Pro M1, 16GB RAM, 2020
* AIR: MacBook Air M1, 8GB RAM, 2020
* NUC: Intel NUC 12th Gen i7-1260P, 32GB RAM
* DO general: 4vCPU/8GB
* DO CPU-opt: 8vCPU/16GB
* DO mem-opt: 4vCPU/32GB

| Block | MBP | AIR | NUC | DO gen | DO/cpu | DO mem |
|-------|-----|-----|-----|--------|--------|--------|
| 24929490 | 77ms | 123ms | 81ms | 130ms | 130ms | 130ms |
| 24929491 | 45ms | 72ms | 62ms | 81ms | 81ms | 82ms |
