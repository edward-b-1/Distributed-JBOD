# Proposal: a human-readable cluster name

Status: proposal for discussion, 19 September 2026. Nothing here is in
SPEC.md yet; section 7 lists what would change there if it is adopted.
Nothing is implemented.

## 1. The problem

A cluster is known only by its UUID (SPEC 6.2.1). Devices and nodes may
carry labels (6.2.5.1), so an operator reads `nas1-bay3` and `nas1`
instead of two UUIDs, but the cluster those belong to is still
`2e79b3df-8d33-4ad1-9eb2-cad1e24a6858`, or `2e79b3df` where the tools
shorten it.

- An operator with more than one cluster, a trial beside production or
  one per site, tells them apart by UUID prefix in `djbod status`,
  `djbod cluster show`, the web UI header and tab title, node logs, and
  the refusal "peer belongs to cluster X, this node to Y" (19.1.5).
- Every client invocation carries `--cluster <uuid>` or `DJBOD_CLUSTER`.
  The UUID is copied from `init-cluster`'s output once and pasted into an
  environment file; nobody types it, but nobody recognises it either.
- The certificate authority is per cluster (19.1.6.1) and the PKI script
  names it after nothing in particular.

None of this is a fault. It is the same gap node labels closed for
nodes, one level up.

## 2. Requirements

1. A name shown beside the UUID wherever the UUID is shown, never instead
   of it: the UUID stays the identity, the name is for people.
2. Set when the cluster is created, changeable afterwards as an ordinary
   document change (6.2.6), so every node and every reader agrees on it.
3. Optional. A document without a name is valid and means an unnamed
   cluster, so existing clusters need nothing.
4. The wrong-cluster protection of `Hello` (19.1.5) is not weakened. Two
   clusters may carelessly be given the same name; they cannot be given
   the same UUID.
5. The same shape rules as labels, for the same reasons: 1 to 128 bytes,
   no whitespace, not shaped like a UUID, so the name can appear in
   commands and environment variables and can never be mistaken for the
   id.

## 3. Where the name lives

**In the cluster document**, as `name: Option<String>` beside
`cluster_id` (6.2.2). The document is what every node holds and every
reader fetches; a rename is a versioned change acknowledged by all
nodes, and a joining node learns the name with everything else.

Alternatives considered:

- *Node configuration.* Each node would carry its own copy and they
  would drift; a rename would be an edit on every machine. Rejected.
- *Device identity file.* Written once at `init-device` and never
  changed (5.2); a rename would be impossible, and a device moved
  between clusters is refused anyway by cluster id. Rejected.

A new document field is exactly the case 6.2.6.4 describes: a node on a
build from before the field refuses a document that carries it. So
`set-name` on a cluster with an older node is refused by that node, with
an error naming it, until it is upgraded; and a cluster created by a new
build with `--name` cannot be joined by an older build. Both are the
rule already stated for rolling upgrades and need nothing new.

## 4. Where the name appears

- `djbod-node init-cluster --name <name>` (`DJBOD_CLUSTER_NAME`, or
  `name` in the configuration file, per 20.6.1) sets it at creation.
- `djbod cluster set-name <name>` and `set-name --clear` change it, as
  `set-node-label` does for a node.
- `djbod status` and `djbod cluster show` print `cluster   <name>
  (<uuid>)`, or the UUID alone when unnamed. `--json` gains `"name"`.
- The `Status` response (19.1.3) gains an optional `cluster_name`, so a
  client shows the name without fetching the document. Old builds ignore
  the field in a response; a new build reads it as absent from an old
  node.
- The web UI header and tab title show the name first with the UUID in
  parentheses, as the nodes table does for labelled nodes.
- The node's "node running" log line and the connection span (20.4.2)
  carry the name beside the cluster id.
- `Hello` (19.1.5) gains an optional `cluster_name`, so the refusal
  "peer belongs to cluster X" can say `production (2e79b3df…)`. Optional,
  ignored by older builds, as `build` is.
- `scripts/djbod-pki.sh init-ca` takes the name for the CA's subject,
  so a certificate says which cluster it was issued for. Cosmetic;
  verification is by CA, not by subject.
- The Compose and systemd examples set `DJBOD_CLUSTER_NAME`.

## 5. Naming the cluster on the client

The one place a name cannot simply stand beside the UUID is the client's
`--cluster` flag. The client sends the cluster id in its `Hello` and the
node refuses a mismatch before anything else happens; a client that knows
only the name has nothing to send. Three ways out:

**(a) The name is for display only; `--cluster` keeps taking the UUID.**
No protocol change. The operator pastes the UUID into `DJBOD_CLUSTER`
once, as now, and sees the name everywhere afterwards. This is the
recommendation for the first step: it delivers everything in section 4
and forecloses nothing.

**(b) A client may ask.** A client `Hello` may carry a nil cluster id,
which a node accepts from clients only (nodes must still match, 6.2.7);
the node's own `Hello` then tells the client the id and name, and the
client closes the connection if the name is not the one it was given.
The protection of 19.1.5 is kept, but it now rests on the name's
uniqueness, which the operator controls, rather than the UUID's, which
nobody does. Two clusters named `nas` would be confusable by exactly the
operator who named them so. A protocol change, small, and a later step
if wanted; `--cluster` would accept either form, telling them apart by
shape (requirement 5).

**(c) A lookup file on the client machine**, mapping names to UUIDs, say
`~/.config/djbod/clusters.toml`. No protocol change and the UUID still
travels in `Hello`, but it is a second copy of the name that can go
stale after a rename, and one more file to distribute. Not recommended;
(b) does the same job from the source of truth.

## 6. Rules

The name uses the label validator (6.2.5.1): 1 to 128 bytes, no
whitespace, not UUID-shaped. Uniqueness across clusters cannot be
enforced by anything inside one cluster and is not claimed. Renaming
changes no data: devices belong to a cluster by UUID (5.2), certificates
are verified by CA, records name nothing about the cluster.

## 7. Changes to SPEC.md if adopted

- 6.2.2: the contents list gains `name`, optional, absent means unnamed.
- New 6.2.5.3 **Cluster name**: the rules of section 6, the commands of
  section 4, and the display rule of requirement 1. Cross-reference from
  6.2.5.1.
- 18.1: `init-cluster --name`; `djbod cluster set-name` beside the other
  document commands.
- 19.1.3: `Status` gains `cluster_name`.
- 19.1.5: `Hello` gains `cluster_name`, informational. If (b) is adopted
  later, the client-only nil cluster id and the check the client makes.
- 20.4.2: the name in the startup line and the connection span.
- 6.2.6.4 can cite the name as an example of a field an older build
  lacks.

## 8. Work if adopted

Small, and shaped like the node-labels change: the field and its
validation in `djbod-core`, `membership::set_cluster_name`, the
`init-cluster` flag, the `set-name` command, the `Status` field, the
`status` and `show` output, and tests mirroring those for node labels.
One PR for the node and CLI; one for the UI header and title; a line in
the PKI script and the deployment examples. Option (b), if wanted,
is its own PR after.

## 9. Open questions

1. Should the name allow spaces, so that `Home NAS` is possible? The
   label rules forbid them so that names work unquoted in commands and
   environment variables and so that no name is mistaken for a UUID.
   Keeping one rule for all three kinds of name seems worth the loss of
   spaces.
2. Is (b) wanted at all, given that `DJBOD_CLUSTER` is set once per
   machine? It buys a friendlier flag at the cost of a protocol change
   and a weaker identity check; the proposal defers it.
3. Should `init-cluster` require a name, so that no cluster is unnamed
   from now on? Existing clusters would still be unnamed until
   `set-name`, so the requirement would not make the tools' unnamed
   case go away. Optional seems right.
