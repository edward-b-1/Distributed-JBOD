#!/usr/bin/env bash
# djbod-pki.sh: the openssl commands of the getting-started guide's TLS
# section (SPEC 19.1.6.1), wrapped for convenience. It creates one
# certificate authority per cluster and issues node and client
# certificates from it. djbod itself generates no keys; this script is
# the administrator's tool, and everything it does can be done by hand
# with openssl.
#
# Usage:
#   djbod-pki.sh init-ca [--name NAME]
#   djbod-pki.sh node <name> <ip>[,<ip>...]
#   djbod-pki.sh client <name>
#   djbod-pki.sh list
#
# Options (before the command):
#   --dir DIR    where the files live; default $DJBOD_PKI_DIR or ./djbod-pki
#   --days N     validity in days; default 3650
#
# Files, in DIR: ca.crt and ca.key (keep the key offline), then
# <name>.crt and <name>.key per node or client. Keys are created readable
# only by their owner, which the node requires. Nothing is ever
# overwritten: delete a file yourself if you mean to reissue it.
set -euo pipefail

usage() {
    sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
    exit "${1:-2}"
}

fail() {
    echo "djbod-pki: $*" >&2
    exit 1
}

DIR="${DJBOD_PKI_DIR:-./djbod-pki}"
DAYS=3650
while [ $# -gt 0 ]; do
    case "$1" in
        --dir) [ $# -ge 2 ] || usage; DIR="$2"; shift 2 ;;
        --days) [ $# -ge 2 ] || usage; DAYS="$2"; shift 2 ;;
        -h|--help) usage 0 ;;
        --*) fail "unknown option $1" ;;
        *) break ;;
    esac
done
[ $# -ge 1 ] || usage
COMMAND="$1"; shift

command -v openssl >/dev/null || fail "openssl is not installed"

refuse_existing() {
    for f in "$@"; do
        [ ! -e "$f" ] || fail "$f exists; delete it first if you mean to reissue"
    done
}

need_ca() {
    [ -f "$DIR/ca.crt" ] && [ -f "$DIR/ca.key" ] || fail "no authority in $DIR; run 'init-ca' first"
}

# An EC P-256 key pair and a certificate request with the given subject
# and extensions, then a certificate signed by the authority. openssl 3
# copies the request's extensions into the certificate.
issue() {
    local name="$1" subject="$2" extensions="$3"
    need_ca
    refuse_existing "$DIR/$name.crt" "$DIR/$name.key"
    local csr
    csr="$(mktemp "$DIR/.$name.XXXXXX.csr")"
    trap 'rm -f "$csr"' RETURN
    (umask 077; openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
        -subj "$subject" $extensions -keyout "$DIR/$name.key" -out "$csr" 2>/dev/null) \
        || fail "openssl could not create the request for $name"
    openssl x509 -req -in "$csr" -CA "$DIR/ca.crt" -CAkey "$DIR/ca.key" -CAcreateserial \
        -days "$DAYS" -copy_extensions copy -out "$DIR/$name.crt" 2>/dev/null \
        || { rm -f "$DIR/$name.key"; fail "openssl could not sign the certificate for $name"; }
    chmod 600 "$DIR/$name.key"
}

is_ip() {
    # IPv4 dotted quad, or anything with a colon (IPv6); openssl validates
    # the rest.
    [[ "$1" =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}$ ]] || [[ "$1" == *:* ]]
}

case "$COMMAND" in
    init-ca)
        NAME="djbod cluster CA"
        while [ $# -gt 0 ]; do
            case "$1" in
                --name) [ $# -ge 2 ] || usage; NAME="$2"; shift 2 ;;
                *) usage ;;
            esac
        done
        mkdir -p "$DIR"
        chmod 700 "$DIR"
        refuse_existing "$DIR/ca.crt" "$DIR/ca.key"
        (umask 077; openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
            -days "$DAYS" -subj "/CN=$NAME" \
            -addext "basicConstraints=critical,CA:TRUE" \
            -addext "keyUsage=critical,keyCertSign,cRLSign" \
            -keyout "$DIR/ca.key" -out "$DIR/ca.crt" 2>/dev/null) \
            || fail "openssl could not create the authority"
        chmod 600 "$DIR/ca.key"
        chmod 644 "$DIR/ca.crt"
        echo "authority created in $DIR"
        echo "  ca.crt  copy to every node and client as the 'ca' path"
        echo "  ca.key  keep here, offline; it signs everything and is needed only to issue"
        ;;
    node)
        [ $# -eq 2 ] || usage
        NAME="$1"; IPS="$2"
        [[ "$NAME" =~ ^[A-Za-z0-9._-]+$ ]] || fail "node name must be letters, digits, '.', '_' or '-'"
        SAN=""
        IFS=',' read -ra LIST <<<"$IPS"
        for ip in "${LIST[@]}"; do
            is_ip "$ip" || fail "'$ip' is not an IP address; the cluster document lists nodes by IP and port, and the certificate must name the same IP (SPEC 19.1.6.1)"
            SAN="${SAN:+$SAN,}IP:$ip"
        done
        issue "$NAME" "/CN=$NAME" "-addext subjectAltName=$SAN -addext extendedKeyUsage=serverAuth,clientAuth"
        echo "node certificate issued: $DIR/$NAME.crt, $DIR/$NAME.key (names $SAN)"
        echo "  copy both, and ca.crt, to the node and set in its configuration:"
        echo "  [tls]"
        echo "  cert = \"/etc/djbod/$NAME.crt\""
        echo "  key = \"/etc/djbod/$NAME.key\""
        echo "  ca = \"/etc/djbod/ca.crt\""
        ;;
    client)
        [ $# -eq 1 ] || usage
        NAME="$1"
        [[ "$NAME" =~ ^[A-Za-z0-9._-]+$ ]] || fail "client name must be letters, digits, '.', '_' or '-'"
        issue "$NAME" "/CN=$NAME" "-addext extendedKeyUsage=clientAuth"
        echo "client certificate issued: $DIR/$NAME.crt, $DIR/$NAME.key"
        echo "  use with: export DJBOD_TLS_CA=$DIR/ca.crt DJBOD_TLS_CERT=$DIR/$NAME.crt DJBOD_TLS_KEY=$DIR/$NAME.key"
        ;;
    list)
        need_ca
        names=() subjects=() expiries=() sans=()
        name_width=20 subject_width=40
        for crt in "$DIR"/*.crt; do
            name="$(basename "$crt" .crt)"
            subject="$(openssl x509 -in "$crt" -noout -subject | sed 's/^subject=//')"
            until="$(openssl x509 -in "$crt" -noout -enddate | sed 's/^notAfter=//')"
            san="$(openssl x509 -in "$crt" -noout -ext subjectAltName 2>/dev/null | tail -n +2 | tr -d ' ' || true)"
            names+=("$name") subjects+=("$subject") expiries+=("$until") sans+=("${san:+[$san]}")
            # Node and client names are ASCII; openssl escapes non-ASCII
            # subject bytes. Size every row before printing any of them.
            if [ "${#name}" -gt "$name_width" ]; then name_width=${#name}; fi
            if [ "${#subject}" -gt "$subject_width" ]; then subject_width=${#subject}; fi
        done
        for i in "${!names[@]}"; do
            printf '%-*s %-*s until %s %s\n' \
                "$name_width" "${names[i]}" "$subject_width" "${subjects[i]}" "${expiries[i]}" "${sans[i]}"
        done
        ;;
    -h|--help) usage 0 ;;
    *) fail "unknown command '$COMMAND'"; ;;
esac
