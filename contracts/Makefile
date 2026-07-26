-include .env
export

RPC        ?= https://sepolia-rpc.giwa.io
FLASH_RPC  ?= https://sepolia-rpc-flashblocks.giwa.io
EXPLORER   ?= https://sepolia-explorer.giwa.io/api
DEPLOYER   ?= 0x5AD176eBb13CAbE62Ee7c07F52a67b4A48CbEf83

# Signing: keystore by default, raw key if PRIVATE_KEY is set. Testnet only for the latter.
ifdef PRIVATE_KEY
  SIGNER := --private-key $(PRIVATE_KEY)
else
  SIGNER := --account dubu-deployer
endif

VERIFY := --verify --verifier blockscout --verifier-url $(EXPLORER)

# forge sizes each broadcast transaction from a simulation in which the WHOLE script runs in one
# EVM, so every account and slot after the first touch is warm. Each transaction then lands cold,
# and on a routed swap (Router -> adapter -> pair -> 2 tokens) the cold-access surcharge exceeds
# the default 130% head-room: measured as an OutOfGas revert on a UniV2 leg at 187,903 gas. Gas is
# 0.001 gwei here and a limit is not a payment, so buy the margin.
GASMUL := --gas-estimate-multiplier 200

.PHONY: build test fmt snapshot cov clean deploy deploy-dry demo demo-dry balance gas engine ci

build:
	forge build

test:
	forge test -vv

# Fuzz/invariant runs are cranked up; use before a release or an audit handoff.
ci:
	FOUNDRY_PROFILE=ci forge test -vv
	cd ../engine && cargo test --all && cargo clippy --all-targets -- -D warnings

fmt:
	forge fmt

snapshot:
	forge snapshot --snap .gas-snapshot

cov:
	forge coverage --report lcov --report summary

clean:
	forge clean && rm -rf cache out

engine:
	cd ../engine && cargo test --all

# Regenerate the Solidity<->Rust differential vectors consumed by test/PropCurve.t.sol
vectors:
	cd ../engine && cargo run --bin gen-vectors

# ---- chain helpers -------------------------------------------------------

balance:
	@echo "deployer: $(DEPLOYER)"
	@cast balance $(DEPLOYER) --rpc-url $(RPC) --ether

gas:
	@echo "gasPrice : $$(cast gas-price --rpc-url $(RPC)) wei"
	@echo "block    : $$(cast block-number --rpc-url $(RPC))"
	@echo "flash    : $$(cast block-number --rpc-url $(FLASH_RPC))"

# The flashblocks endpoint only exposes preconfirmed state under the "pending" tag; its
# "latest" lags the ordinary RPC. Anything latency-sensitive must query "pending" here.
pending:
	@cast rpc eth_getBlockByNumber pending false --rpc-url $(FLASH_RPC) | jq '{number,hash,timestamp}'

# ---- deployment ----------------------------------------------------------

deploy:
	forge script script/Deploy.s.sol:Deploy \
		--rpc-url $(RPC) $(SIGNER) --broadcast $(VERIFY) $(GASMUL) -vvv

# Dry run against live chain state. Sends nothing, needs no key. Same output as `deploy`.
deploy-dry:
	forge script script/Deploy.s.sol:Deploy --rpc-url $(RPC) --sender $(DEPLOYER) -vvv

# Seeds both venues with equal notional TVL and runs the comparison sweep.
# Set DEMO_MARKETS=1 for mWETH/mUSDC only, 2 for mWBTC/mUSDC only, 3 (default) for both.
demo:
	forge script script/Demo.s.sol:Demo \
		--rpc-url $(RPC) $(SIGNER) --broadcast $(GASMUL) -vvv

demo-dry:
	forge script script/Demo.s.sol:Demo --rpc-url $(RPC) --sender $(DEPLOYER) -vvv
