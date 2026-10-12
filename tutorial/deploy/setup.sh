#!/bin/sh
# The server's one-time setup: `just setup` (see "Setting it up" in
# tutorial/README.md). Run it as yourself; it asks sudo for what needs root,
# and running it again is harmless.
#
# - deploy/.env: made from .env.example, with DOMAIN (this machine's name)
#   and the way attendees reach it: when something already serves HTTPS here
#   (port 443 taken), COMPOSE_PROFILES=plain behind it; otherwise Caddy does
#   TLS itself (TLS=auto, Let's Encrypt). It asks for HUB_ADMINS while that
#   is empty.
# - Docker access for you (the docker group).
# - The session token's directory and group (SHARED_DIR, SHARED_GID).
# - With plain behind Apache (Debian's apache2): `sh setup.sh apache` (on
#   its own, too) points the virtual host that serves https://DOMAIN/ at
#   Caddy. It writes the proxy settings to one file of their own and adds one
#   Include line to that virtual host, keeps a copy of the file it edits, and
#   puts back what it changed if Apache rejects the result.
set -eu

deploy=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
cd "$deploy"
# a2enmod and apache2ctl live there, off a user's PATH on Debian.
PATH=$PATH:/usr/sbin:/sbin
sudo=sudo
[ "$(id -u)" -ne 0 ] || sudo=
user=$(id -un)
relogin=no

get() { sed -n "s/^$1=//p" .env | tail -1; }
put() {
    if grep -q "^$1=" .env; then
        sed -i "s|^$1=.*|$1=$2|" .env
    else
        printf '%s=%s\n' "$1" "$2" >> .env
    fi
}
in_group() { id -nG "$user" | tr ' ' '\n' | grep -qx "$1"; }

# Apache in front of Caddy: the virtual host for https://DOMAIN/ gets an
# Include of the proxy settings.
apache() {
    domain=$(get DOMAIN)
    port=$(get PLAIN_PORT); port=${port:-8000}
    snippet=/etc/apache2/cactup-tutorial-proxy.conf
    include="    Include $snippet"
    if [ ! -d /etc/apache2/sites-enabled ] || ! command -v a2enmod > /dev/null; then
        echo "apache: no Debian-style Apache here (/etc/apache2, a2enmod); set up the proxy by hand (README: TLS)" >&2
        return 1
    fi
    # The enabled site file with a :443 virtual host named DOMAIN.
    vhost=
    for f in /etc/apache2/sites-enabled/*; do
        if awk -v d="$domain" '
            /^[ \t]*<VirtualHost[^>]*:443>/ { in443 = 1 }
            /^[ \t]*<\/VirtualHost>/ { in443 = 0 }
            in443 && tolower($1) ~ /^server(name|alias)$/ { for (i = 2; i <= NF; i++) if ($i == d) found = 1 }
            END { exit !found }' "$f"; then
            vhost=$(readlink -f "$f")
            break
        fi
    done
    if [ -z "$vhost" ]; then
        echo "apache: no enabled site has a :443 virtual host named $domain (DOMAIN in deploy/.env)." >&2
        echo "Add to the one that serves https://$domain/:" >&2
        echo "$include" >&2
        apache_settings >&2
        return 1
    fi
    echo "apache: $vhost serves https://$domain/: proxying it to Caddy on 127.0.0.1:$port"
    # What this run changes, to put back if Apache rejects it.
    previous=$(mktemp)
    had_snippet=no
    if [ -f "$snippet" ]; then
        cp "$snippet" "$previous"
        had_snippet=yes
    fi
    edited=no
    tmp=$(mktemp)
    apache_settings > "$tmp"
    $sudo install -m 0644 "$tmp" "$snippet"
    rm -f "$tmp"
    if ! grep -qF "Include $snippet" "$vhost"; then
        edited=yes
        $sudo cp -p "$vhost" "$vhost.before-cactup-tutorial"
        # Before the end of the :443 virtual host named DOMAIN.
        edited=$(mktemp)
        awk -v d="$domain" -v inc="$include" '
            /^[ \t]*<VirtualHost[^>]*:443>/ { in443 = 1; named = 0 }
            in443 && tolower($1) ~ /^server(name|alias)$/ { for (i = 2; i <= NF; i++) if ($i == d) named = 1 }
            /^[ \t]*<\/VirtualHost>/ { if (in443 && named && !done) { print inc; done = 1 } in443 = 0 }
            { print }' "$vhost" > "$edited"
        $sudo cp "$edited" "$vhost"
        rm -f "$edited"
        echo "apache: added the Include to $vhost (the original: $vhost.before-cactup-tutorial)"
    fi
    $sudo a2enmod -q proxy proxy_http headers
    if ! $sudo apache2ctl configtest; then
        echo "apache: Apache rejects the result; putting back what this changed" >&2
        [ "$edited" = no ] || $sudo cp -p "$vhost.before-cactup-tutorial" "$vhost"
        if [ "$had_snippet" = yes ]; then
            $sudo install -m 0644 "$previous" "$snippet"
        else
            $sudo rm -f "$snippet"
        fi
        rm -f "$previous"
        return 1
    fi
    rm -f "$previous"
    $sudo systemctl reload apache2
    echo "apache: reloaded; https://$domain/ answers once 'just up' has started Caddy (503 until then)"
}

apache_settings() {
    cat <<EOF
# The Cactup tutorial's hub, behind this virtual host: written by
# tutorial/deploy/setup.sh (\`just setup\`), and included by the virtual host
# that serves https://$domain/. To undo: remove that Include line.
ProxyPreserveHost On
RequestHeader set X-Forwarded-Proto https
# Kernels and terminals keep WebSockets open for hours.
ProxyTimeout 3600
# The certificate's renewal (certbot's webroot) keeps working over HTTPS.
ProxyPass /.well-known/acme-challenge/ !
ProxyPass        / http://127.0.0.1:$port/ upgrade=websocket
ProxyPassReverse / http://127.0.0.1:$port/
EOF
}

if [ "${1:-}" = apache ]; then
    apache
    exit
fi

# -- deploy/.env ------------------------------------------------------------------
if [ ! -f .env ]; then
    cp .env.example .env
    domain=$(hostname -f 2>/dev/null || hostname)
    put DOMAIN "$domain"
    if ss -ltnH 'sport = :443' 2>/dev/null | grep -q .; then
        put COMPOSE_PROFILES plain
        echo "made deploy/.env: DOMAIN=$domain; port 443 is already served here, so Caddy goes behind it (COMPOSE_PROFILES=plain)"
    else
        echo "made deploy/.env: DOMAIN=$domain; Caddy serves HTTPS with a Let's Encrypt certificate (TLS=auto)"
    fi
fi

# The hub's admins: usernames on the hub (the authenticator's rule), whose
# accounts `just account` makes.
while [ -z "$(get HUB_ADMINS)" ] && [ -t 0 ]; do
    suggest=$(printf '%s' "$user" | tr 'A-Z' 'a-z')
    printf '%s\n' "$suggest" | grep -Eqx '[a-z][a-z0-9-]{1,31}' || suggest=
    printf 'The hub'"'"'s admins: usernames on the hub, comma-separated [%s]: ' "$suggest"
    read -r admins || break
    admins=$(printf '%s' "${admins:-$suggest}" | sed 's/^ *//; s/ *$//; s/ *, */,/g' | tr 'A-Z' 'a-z')
    if printf '%s\n' "$admins" | grep -Eqx '[a-z][a-z0-9-]{1,31}(,[a-z][a-z0-9-]{1,31})*'; then
        put HUB_ADMINS "$admins"
    else
        echo "  each: a lowercase letter, then 1 to 31 lowercase letters, digits or dashes"
    fi
done

# -- Docker ---------------------------------------------------------------------------
rootless=no
if docker info > /dev/null 2>&1; then
    case $(docker info --format '{{.SecurityOptions}}') in *rootless*) rootless=yes ;; esac
elif ! command -v docker > /dev/null; then
    echo "setup: no Docker here: install Docker Engine first (README: Setting it up)" >&2
    exit 1
elif [ -n "$sudo" ] && getent group docker > /dev/null && ! in_group docker; then
    echo "setup: adding $user to the docker group (Docker access: as powerful as root on this machine)"
    $sudo usermod -aG docker "$user"
    relogin=yes
elif ! in_group docker; then
    echo "setup: Docker isn't answering (docker info): is it running?" >&2
    exit 1
fi

# -- the session token's directory -------------------------------------------------------
if [ "$rootless" = yes ]; then
    # The hub's container keeps none of your groups: a directory of your own.
    shared=$(get SHARED_DIR)
    case $shared in /srv/cactup-tutorial | "") shared=${XDG_DATA_HOME:-$HOME/.local/share}/cactup-tutorial ;; esac
    install -d -m 0750 "$shared"
    put SHARED_DIR "$shared"
    put SHARED_GID 0
else
    shared=$(get SHARED_DIR); shared=${shared:-/srv/cactup-tutorial}
    getent group cactup-tutorial > /dev/null || $sudo groupadd cactup-tutorial
    if [ -n "$sudo" ] && ! in_group cactup-tutorial; then
        $sudo usermod -aG cactup-tutorial "$user"
        relogin=yes
    fi
    [ -d "$shared" ] || $sudo install -d -m 2770 -g cactup-tutorial "$shared"
    put SHARED_GID "$(getent group cactup-tutorial | cut -d: -f3)"
fi
echo "setup: the session token lives in $shared"

# -- the web server in front --------------------------------------------------------------
front=ok
if [ "$(get COMPOSE_PROFILES)" = plain ] && systemctl is-active -q apache2 2>/dev/null; then
    # A process of its own: in a function called before ||, set -e is off.
    sh "$deploy/setup.sh" apache || front=failed
elif [ "$(get COMPOSE_PROFILES)" = plain ]; then
    echo "setup: COMPOSE_PROFILES=plain: point the web server in front at 127.0.0.1:$(get PLAIN_PORT) (README: TLS)"
fi

echo
if [ "$front" = failed ]; then
    echo "setup: all done but Apache's part, which failed (above): fix that, then 'sh deploy/setup.sh apache'" >&2
fi
echo "Check deploy/.env, then:"
[ "$relogin" = no ] || echo "  log out and in again (new groups)"
echo "  just up"
for admin in $(get HUB_ADMINS | tr ',' ' '); do
    echo "  just account $admin"
done
echo "  just token"
[ "$front" = ok ]
