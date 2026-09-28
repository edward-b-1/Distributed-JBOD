# TLS

Out of the box every connection is plain TCP. That is appropriate on a
network whose members you trust. Anywhere else, issue certificates and
switch the cluster's transport. The node signs nothing and generates no
keys. The files are yours. `scripts/djbod-pki.sh` is a wrapper around
the `openssl` commands below. There is still no per-user authorisation:
any client with a certificate from this authority can do every
operation, including administration.

The transport has three values.

| Value | What connects |
|---|---|
| `plain` | TCP with no TLS. A node that has certificates loaded also accepts TLS. |
| `tls-optional` | Nodes speak TLS to each other. A client may connect in the clear, or with TLS and no client certificate. |
| `tls` | TLS only, and the client must present a certificate. |

Move one step at a time. Each step is refused while any node has no
certificate loaded, and the error names that node.

## Create an authority

Once per cluster, on a machine you can then take the key off:

```sh
scripts/djbod-pki.sh --dir ~/djbod-pki init-ca --name home-nas
```

This writes `ca.crt` and `ca.key`. The key is mode 0600. The script
refuses to overwrite either file. `ca.key` is only needed to issue
further certificates. Copy `ca.crt` to every node. Leave `ca.key` off
the nodes.

The equivalent `openssl` invocation is an EC P-256 self-signed
certificate with `basicConstraints=critical,CA:TRUE` and
`keyUsage=critical,keyCertSign,cRLSign`. `--days` defaults to 3650.

## A certificate for each node

The certificate must contain the node's address as an IP subject
alternative name. Peers and clients check the certificate against the
address they dialled, which is the address in the cluster document.
The script refuses a name that is not an IP address. A node with more
than one address takes them comma-separated.

```sh
scripts/djbod-pki.sh --dir ~/djbod-pki node nas1 10.0.0.1
scripts/djbod-pki.sh --dir ~/djbod-pki node nas2 10.0.0.2
scripts/djbod-pki.sh --dir ~/djbod-pki client admin
scripts/djbod-pki.sh --dir ~/djbod-pki list
```

A node certificate has extended key usage `serverAuth,clientAuth`,
because the node is a server for clients and a client when it dials its
peers. A client certificate has `clientAuth` only. Keys are mode 0600.
The node refuses to start if its key is readable by anyone else.

Install `nas1.crt`, `nas1.key`, and `ca.crt` on that machine, readable
by the user the node runs as, and add them to the configuration:

```toml
[tls]
cert = "/etc/djbod/nas1.crt"
key = "/etc/djbod/nas1.key"
ca = "/etc/djbod/ca.crt"
```

The same three paths can be `--tls-cert`, `--tls-key`, and `--tls-ca`,
or `DJBOD_TLS_CERT`, `DJBOD_TLS_KEY`, and `DJBOD_TLS_CA`. A flag wins
over a variable, which wins over the file. Restart the node after the
files are in place. `djbod status` still says `transport plain`. The
node is willing to speak TLS and is not yet requiring it.

Trying the switch before the restart fails, and names the node:

```text
node <uuid> at 10.0.0.1:5263 has no TLS material loaded; give it [tls] paths and restart it before moving the transport off plain
```

## Switch the transport

With every node restarted and holding its certificate:

```sh
export DJBOD_TLS_CA=~/djbod-pki/ca.crt
export DJBOD_TLS_CERT=~/djbod-pki/admin.crt
export DJBOD_TLS_KEY=~/djbod-pki/admin.key

djbod cluster set-transport tls-optional
djbod status
```

`status` now says `transport tls-optional`. A client with only
`DJBOD_TLS_CA` set is encrypted and presents no certificate, which
`tls-optional` accepts. Confirm that works, then require certificates:

```sh
djbod cluster set-transport tls
```

A client with the three variables unset is refused. The error is
`TlsRequired` and the text `this cluster's transport is tls; plain
connections are refused`. With the three variables set, `djbod status`
works and a `put` and `get` work.

`djbod cluster set-transport plain` goes back. Do that only when you
mean to, and remember that plain traffic is unauthenticated.

## A new node

Issue its certificate, put the `[tls]` table in its configuration, and
`djbod-node join` as in [Deployment](deployment.md#three-servers). Join
tries TLS first when the config has certificates. After `join`, start
the node. `djbod cluster show` should list it at the same document
version as the others. If the new node's certificate is not the one the
peers expect, `cluster show` prints `unreachable:` and the TLS error in
that node's row. Fix the files and restart that node. The rest of the
cluster keeps the document it already has.

## Withdrawing a certificate

A signature cannot be taken back. To drop a lost laptop or a
decommissioned node, create a new authority, put both CA certificates
in every `ca.crt` during the overlap (a PEM file may hold several),
restart the nodes, issue new certificates to whoever should remain, then
remove the old CA certificate from the bundles and restart again.
Expiry is the same operation: new files, then a restart. Removing a
node from the cluster does not withdraw its certificate.

`djbod-pki.sh` will not overwrite an existing key or certificate. Delete
the file yourself when you mean to reissue it. `list` prints each
certificate's subject, expiry, and subject alternative names.
