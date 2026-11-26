# Discovery

For a bare server name, the client requests
`GET https://NAME/.well-known/rargo.json`. The response is JSON with
`server` (the advertised host and port), `protocol` (an integer),
`version` (the server release), `auth` and `github` (booleans). The
binary link goes to the advertised address. A configured address with
a port gives the client the binary address directly and uses HTTP for
its authentication pages. A public HTTPS proxy serves discovery and
the authentication pages and forwards those requests to the server.

# Credentials and link agreement

A token is an identifier and secret separated by a dot. Use the
identifier's UTF-8 bytes in the handshake. Derive the 32-byte
pre-shared key with BLAKE3 derive-key in context
`rargo client key v1` over the secret with ASCII whitespace trimmed.
Both transports use Noise `NNpsk0_25519_ChaChaPoly_BLAKE2s`.
The client sends the first Noise message and the server sends the
response. The resulting transport keys are directional.

## UDP

A client picks a random nonzero odd connection ID. Outside the
handshake, datagrams authenticate the connection ID, the packet header,
and payload with the first 16 bytes of a keyed BLAKE3 hash. The tag key
is BLAKE3 derive-key over the Noise handshake hash in context
`rargo link datagram tag v1`. Only authenticated traffic updates the
peer address after the agreement.

All packet integers are little-endian. The packet body is one of these
layouts, followed by the tag once the connection is established.

| Octets | Data packet | Acknowledgement packet |
| --- | --- | --- |
| 0 | Type 1 | Type 2 |
| 1–8 | Connection ID (u64) | Connection ID (u64) |
| 9 onward | Message ID (u64), zero-based fragment number (u32), fragment count (u32), fragment bytes | Repeated message ID (u64) and fragment number (u32) pairs |

Message ID 1 carries the agreement in each direction. Its first
payload is a one-byte identifier length, the identifier, the eight
magic octets (as for TCP below) and the first Noise message. The Noise prologue is `rargo worker link v2` and the
handshake PSK is BLAKE3 derive-key in context
`rargo client link handshake v1` over the token PSK. The response
starts with status 0 followed by the Noise response, or status 1
followed by a UTF-8 refusal. A server refuses a hello without the
magic octets, or with another protocol number, with text that names
the release to install. A client whose marked hello is refused with
the plain refusal of an older server retries once without the magic
octets, from a fresh socket.

After key agreement, seal each complete message before fragmenting it.
The sealed message starts with the first nonce as u64 and contains
consecutive Noise records with 65,000 bytes of plaintext in each
record except the last. Each record has a 16-byte authentication tag.
The receiver reassembles 1,200-byte fragments by their numbers and
acknowledges each accepted fragment. Resend unacknowledged fragments
and suppress duplicate deliveries. Complete messages may arrive out
of order. The connection shares its congestion window across commands.

## TCP fallback

Use the same host and port over TCP when the default UDP attempt
produces no answer in 2.5 seconds. An explicit authentication refusal
ends the attempt. The client starts TCP with the eight magic octets
(ASCII `rargo`, the protocol number as one byte, 9 for this
version, CR, LF), a one-byte
identifier length, the identifier, a little-endian u16 Noise message
length and the Noise
message. TCP uses the token PSK directly and an empty Noise prologue.
The response uses a status byte and little-endian u16 length before the
Noise response or refusal text.

Each subsequent TCP record has a little-endian u32 ciphertext length.
Its encrypted plaintext is one byte (1 for the last record in the
message, 0 otherwise) followed by up to 65,000 bytes of message.
Start the Noise nonce at zero in each direction and advance it for
each record. Concatenate plaintext fragments until the last-record
flag to recover a frame.

# Frames and commands

A transport message contains one bincode 1 frame. Fixed-width integers
and discriminants use little-endian encoding. Enum discriminants are
zero-based u32 values in declaration order. Strings and vectors use a
u64 length, followed by their bytes or elements. Byte arrays have no
length prefix. A boolean is one byte and an option has a one-byte tag.
`Packed` contains another encoded message of the same direction,
compressed with Zstandard.

A frame begins with a u32 stream ID and u64 sequence number. Stream 0
carries offers and control operations. A command uses its own stream.
Client frames share a sequence starting at zero across streams, so
offers precede any commands that rely on them. Each server stream has
its own sequence starting at zero. Stream 4294967295 carries
`Ping` and `Pong` immediately, outside ordered dispatch.

To build, send `Run` with its protocol version, argument vector, client
paths, environment, worktree identity and root tree digest. Answer
`WantEnv` with `Env`, where an empty option means the variable is
unset. `Stdout` and `Stderr` carry byte streams.
`Deliver` describes artifacts and filesystem writes and lists the
target directories; refuse any path outside the worktree root and
those directories. Handle those
writes before reporting the final `Exit` or starting the process in
`Exec`. `Cancel` withdraws a command whose caller has gone away.
`Report` carries a future-incompatibility report for the report file
at its path; add it there and answer with `ReportId`, the number it
received, which replaces the placeholder (`report-id` between two U+0001 characters) in its
suggestion text.

# Content sync and results

A directory manifest is a vector of byte names and nodes sorted by
name. Each name is a single, nonempty path component. A file node
contains the BLAKE3 digest of the bytes, size and executable bit.
A symlink contains target bytes and a directory node contains another
manifest digest. Hash the encoded directory manifest using BLAKE3.
The root digest names the snapshot.

`Offer` can push new manifests and blobs before the command.
An offer with `ask` gets a `Missing` reply with the server's missing
digests. On the command stream, answer `WantTrees` with `Trees`
and `WantBlobs` with `Blobs` or `BlobPart`. Use `Changed` if source
content vanishes or changes before upload. The server checks it holds
the complete snapshot before starting the build.

For copied outputs, `Deliver` names the files, the server sends blobs
it expects the client to need, and `Delivered` ends that batch. Ask
for any remaining digest with `Fetch` and wait for its `Fetched`
marker. Echo a `DeliveryToken` in the next request for the worktree
after placing its complete delivery. For lazy outputs, the listing
can arrive during the build. The client fetches content as it is read.
For content of 1 MiB or more it asks for the `Recipe`, which also carries
the content's outline (the BLAKE3 chaining values of its 64 KiB groups,
which give the content hash) and the byte ranges of its debug sections,
then indexed `Chunks`, reusing chunks it already holds. It serves a group
to a reader only once the group's bytes give its chaining value. `Blob.packed` and `Part.packed` select Zstandard compression
for that content.

# Browser authentication

1. POST JSON `{"machine":"...","protocol":N}` to `/auth/start`, where
   N is the client's protocol number. Another number, or none, gets
   HTTP 409 with JSON `{"error":"..."}` naming the release to install.
   Otherwise keep the
   returned `device` secret in the terminal and show the short
   `code`. Open the returned relative `path` in the browser; it
   carries `protocol` too, and the page refuses another number. The
   response also gives the poll `interval` and `expires_in`, both in
   seconds.
2. Confirm the code. For an invitation, the browser posts form fields
   `code`, `invite` and optional `name` to `/auth/approve`. For GitHub,
   it follows `/auth/github?code=...`, then the OAuth callback at
   `/auth/github/callback`. The server matches the callback's state
   to the pending confirmation and checks the account allow list.
3. Poll `/auth/poll` with JSON `{"device":"..."}` at that interval,
   stopping when the request expires. Status is `pending`,
   `denied`, `expired` or `approved`. An approval carries `token`
   and `user` and can be picked up once. Store the token securely.

POST JSON `{"token":"..."}` to `/auth/whoami` to read the holder
(`user`, null for an invalid token), or to `/auth/revoke` to revoke it
(`revoked`, a boolean). These HTTP calls and the browser flow are
separate from the binary transport.
