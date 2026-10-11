#!/bin/sh
# Caddy's entrypoint: splits SITE_PROXY into what the Caddyfiles use, then
# runs Caddy with the mode's Caddyfile (CADDY_MODE: auto, provided, plain).
#
#   run.sh            run Caddy (the container's command)
#   run.sh reload     reload the config and the certificate files
#                     (`docker compose exec caddy sh /etc/caddy/run.sh reload`)
set -eu

url=${SITE_PROXY:-}
url=${url%/}
if [ -n "$url" ]; then
    case $url in
        http://*|https://*) ;;
        *) echo "SITE_PROXY=$SITE_PROXY: give a URL, like https://example.org/docs/" >&2; exit 1 ;;
    esac
    rest=${url#*://}
    SITE_PROXY_HOST=${rest%%/*}
    SITE_PROXY_ORIGIN=${url%%://*}://$SITE_PROXY_HOST
    case $rest in */*) SITE_PROXY_PATH=/${rest#*/} ;; *) SITE_PROXY_PATH= ;; esac
    SITE_MODE=proxy
else
    SITE_PROXY_ORIGIN= SITE_PROXY_PATH= SITE_MODE=hub
fi
export SITE_PROXY_ORIGIN SITE_PROXY_PATH SITE_MODE

config=/etc/caddy/Caddyfile.${CADDY_MODE:?}
if [ "${1:-}" = reload ]; then
    exec caddy reload --force --config "$config" --adapter caddyfile
fi
exec caddy run --config "$config" --adapter caddyfile
