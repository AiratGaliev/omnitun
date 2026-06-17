#!/bin/sh
set -e

CERT_DIR="/etc/sing-box/certs"
WG_DIR="/etc/sing-box/wg-keys"

# === TLS certificates (for Hysteria2 and TUIC) ===
CERT_FILE="$CERT_DIR/cert.pem"
KEY_FILE="$CERT_DIR/key.pem"

if [ ! -f "$CERT_FILE" ] || [ ! -f "$KEY_FILE" ]; then
    echo "[entrypoint] Generating TLS certificate for localhost..."
    mkdir -p "$CERT_DIR"
    openssl req -x509 -newkey rsa:2048 -nodes \
        -keyout "$KEY_FILE" \
        -out "$CERT_FILE" \
        -days 365 \
        -subj "/CN=localhost" \
        -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" \
        2>/dev/null
    echo "[entrypoint] TLS certificate generated"
    chmod 644 "$CERT_FILE"
    chmod 600 "$KEY_FILE"
else
    echo "[entrypoint] Using existing TLS certificate"
fi

# === WireGuard keys ===
WG_PRIVATE_KEY="$WG_DIR/private.key"
WG_PUBLIC_KEY="$WG_DIR/public.key"

if [ ! -f "$WG_PRIVATE_KEY" ]; then
    echo "[entrypoint] Generating WireGuard keys..."
    mkdir -p "$WG_DIR"

    # Generating a private key
    if command -v wg >/dev/null 2>&1; then
        wg genkey > "$WG_PRIVATE_KEY" 2>/dev/null
        wg pubkey < "$WG_PRIVATE_KEY" > "$WG_PUBLIC_KEY" 2>/dev/null
    else
        # Fallback: generate via sing-box
        sing-box generate wg-key > "$WG_PRIVATE_KEY" 2>/dev/null
        cat "$WG_PRIVATE_KEY" | sing-box generate wg-key -pub > "$WG_PUBLIC_KEY" 2>/dev/null || true
    fi

    chmod 600 "$WG_PRIVATE_KEY"
    chmod 644 "$WG_PUBLIC_KEY"

    echo ""
    echo "=== WIREGUARD PUBLIC KEY (скопируй в клиент) ==="
    cat "$WG_PUBLIC_KEY"
    echo ""
else
    echo "[entrypoint] Using existing WireGuard keys"
    echo ""
    echo "=== WIREGUARD PUBLIC KEY ==="
    cat "$WG_PUBLIC_KEY"
    echo ""
fi

# === Paste the WireGuard private_key into the config ===
WG_PRIVATE=$(cat "$WG_PRIVATE_KEY")
sed "s|__WG_PRIVATE__|$WG_PRIVATE|" /etc/sing-box/config.json > /tmp/config.json
echo "[entrypoint] Config patched with WireGuard key"

exec sing-box run -c /tmp/config.json