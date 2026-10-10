#!/bin/sh
# Existing operator material only. No credential generation or enrollment.
set -eu
umask 077
: "${GATEWAY_HOSTED_CONFIG_JSON:?missing hosted config}"
: "${GATEWAY_HOSTED_BISCUIT:?missing gateway Biscuit}"
: "${GATEWAY_HOSTED_SIGNER_PEM:?missing existing signer}"
: "${GATEWAY_HOSTED_SOURCE_AUTHOR_JSON:?missing native SourceAuthor}"
: "${GATEWAY_ARTIFACTS_CREDENTIAL:?missing existing Artifacts credential}"
mkdir /tmp/heddle-hosted
mkdir /tmp/heddle-hosted/scratch
printf '%s' "$GATEWAY_HOSTED_CONFIG_JSON" > /tmp/heddle-hosted/config.json
printf '%s' "$GATEWAY_HOSTED_BISCUIT" > /tmp/heddle-hosted/biscuit
printf '%s' "$GATEWAY_HOSTED_SIGNER_PEM" > /tmp/heddle-hosted/signer.pem
printf '%s' "$GATEWAY_HOSTED_SOURCE_AUTHOR_JSON" > /tmp/heddle-hosted/source-author.json
printf '%s' "$GATEWAY_ARTIFACTS_CREDENTIAL" > /tmp/heddle-hosted/artifacts-credential
if [ -n "${GATEWAY_HOSTED_MINT_ATTACHMENT_JSON:-}" ]; then
  printf '%s' "$GATEWAY_HOSTED_MINT_ATTACHMENT_JSON" > /tmp/heddle-hosted/mint-attachment.json
fi
unset GATEWAY_HOSTED_CONFIG_JSON GATEWAY_HOSTED_BISCUIT GATEWAY_HOSTED_SIGNER_PEM
unset GATEWAY_HOSTED_SOURCE_AUTHOR_JSON GATEWAY_ARTIFACTS_CREDENTIAL GATEWAY_HOSTED_MINT_ATTACHMENT_JSON
exec /usr/local/bin/gateway_host --hosted /tmp/heddle-hosted/config.json --bind 0.0.0.0:8080
