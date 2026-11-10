.PHONY: firefly clear-firefly build test \
        register rotate admin-relink resolve-current resolve-history keygen

# Spin up a local F1r3fly shard via the workspace's docker compose. Copies
# docker/.env from the shipped example on first run.
firefly:
	@if [ ! -f ../docker/.env ] && [ -f ../docker/.env.example ]; then \
		cp ../docker/.env.example ../docker/.env; \
	fi
	docker compose -f ../docker/shard.yml up

# Stop the shard and wipe all node data volumes. Useful when finality has
# fallen behind block production and the proposer starts refusing new blocks
# because too many unfinalized blocks have piled up above the last finalized
# tip. A fresh genesis resets the finalized tip to block 0 so reruns work
# again. Run between test campaigns when deploys pile up.
clear-firefly:
	docker compose -f ../docker/shard.yml down -v

build:
	cargo build --release

# Identity registry REG-AC suite. Requires a running F1R3FLY cluster on
# 127.0.0.1:40422 and F1R3FLY_SIGNING_KEY in the environment.
test:
	cargo test --features integration_tests --test identity_registry -- --test-threads=1

# Registry CLI targets. All require F1R3FLY_SIGNING_KEY in the environment.
# IDENTITY_REGISTRY_URI defaults to the genesis registry
# (rho:system:identityRegistry).
# Op-specific args are passed as make variables.
#
#   make keygen
#   make register
#   make register SK=<32-byte hex> NONCE=<32-byte hex>
#   make rotate NAME=<32-byte hex> CURRENT_SK=<32-byte hex> NEW_SK=<32-byte hex> SEQ=<n>
#   make admin-relink NAME=<32-byte hex> NEW_SK=<32-byte hex> SEQ=<n> EVIDENCE=<hex>
#   make resolve-current NAME=<32-byte hex>
#   make resolve-history NAME=<32-byte hex>

REGISTER_ARGS := $(if $(SK),--sk $(SK)) $(if $(NONCE),--nonce $(NONCE))

register:
	cargo run --bin registry_cli -- register $(REGISTER_ARGS)

rotate:
	cargo run --bin registry_cli -- rotate \
		--name $(NAME) \
		--current-sk $(CURRENT_SK) \
		--new-sk $(NEW_SK) \
		--seq $(SEQ)

admin-relink:
	cargo run --bin registry_cli -- admin-relink \
		--name $(NAME) \
		--new-sk $(NEW_SK) \
		--seq $(SEQ) \
		--evidence $(EVIDENCE)

resolve-current:
	cargo run --bin registry_cli -- resolve-current --name $(NAME)

resolve-history:
	cargo run --bin registry_cli -- resolve-history --name $(NAME)

keygen:
	cargo run --bin registry_cli -- keygen
