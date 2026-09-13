# Running the eth-rpc Module Against logoscore

`logos-evm-eth-rpc-module` is the **proxyable, fail-closed Ethereum JSON-RPC
client** for the Logos multi-chain EVM wallet. It stores configuration per
chain (endpoint + proxy policy) so callers route by `chainId` alone, and every
outbound request is built through a single fail-closed chokepoint: a chain
configured with `proxyRequired` and no usable proxy **refuses to send** rather
than leaking in the clear.

This doc-test drives the module through a `logoscore` daemon against a **local
mock JSON-RPC node** (so it needs no external network and reproduces in CI):

1. Build/install the module and start a daemon.
2. Configure a chain pointing at the mock node and read a balance — a real
   round-trip through the module's RPC transport.
3. Configure a second chain that **requires a proxy** but has none, and watch
   the module refuse the request (the privacy guarantee).

**What you'll build:** This `eth_rpc_module`, packaged as `.lgx`, installed with `lgpm`, and driven through a `logoscore` daemon against a local mock node.

**What you'll learn:**

- How per-chain config is stored in the module and addressed by chainId
- How a JSON-RPC round-trip flows through the module's transport
- How the fail-closed proxy chokepoint refuses to send when a proxy is required but unavailable

## Prerequisites

- **Nix** with flakes enabled. Install from [nixos.org](https://nixos.org/download.html), then enable flakes:

```bash
mkdir -p ~/.config/nix
echo 'experimental-features = nix-command flakes' >> ~/.config/nix/nix.conf
```

- **A Linux or macOS machine** with `python3` available (used to run the local mock JSON-RPC node).

---

## Step 1: Build logoscore and lgpm

### 1.1 Build logoscore

`eth_rpc_module` is `concurrency: "multi"`, so the daemon resolves its
deferred replies on the caller's behalf — it must be built against
logos-protocol ≥ 0.2. The `--override-input` pins its protocol (and
liblogos's) to the chain under test (master, where 0.2 lives).

```bash
nix build 'github:logos-co/logos-logoscore-cli#cli' --out-link ./logos
```

### 1.2 Build lgpm

```bash
nix build 'github:logos-co/logos-package-manager#cli' -o lgpm
```

---

## Step 2: Build and install the eth-rpc module

### 2.1 Build the module's .lgx

```bash
nix build 'github:logos-co/logos-evm-eth-rpc-module#lgx' -o eth-rpc-lgx
```

```bash
ls eth-rpc-lgx/*.lgx
```

### 2.2 Seed the capability module

```bash
mkdir -p modules
cp -RL ./logos/modules/. ./modules/

```

### 2.3 Install the .lgx with lgpm

```bash
./lgpm/bin/lgpm --modules-dir ./modules --allow-unsigned install --file eth-rpc-lgx/*.lgx
```

### 2.4 Confirm the install

```bash
./lgpm/bin/lgpm --modules-dir ./modules list
```

---

## Step 3: Start a mock JSON-RPC node

A tiny local node that answers a few JSON-RPC methods with canned values, so
the round-trip is deterministic and offline.

### 3.1 Write the mock node

```
import http.server, json, socketserver, time
RES = {"eth_chainId": "0x1", "eth_getBalance": "0x1234", "eth_blockNumber": "0x10"}
class H(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        n = int(self.headers.get('content-length', 0))
        req = json.loads(self.rfile.read(n) or b'{}')
        body = json.dumps({"jsonrpc": "2.0", "id": req.get("id", 1),
                           "result": RES.get(req.get("method"), "0x0")}).encode()
        self.send_response(200)
        self.send_header('content-length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *a): pass
# Threaded because eth_rpc_module is concurrency: "multi" and opens a fresh
# connection per call, so the single-threaded server would serialise them.
class Node(http.server.ThreadingHTTPServer):
    # HTTPServer.server_bind() looks the host up in reverse DNS BETWEEN bind() and
    # listen(), and nothing here needs the FQDN. Until that lookup answers the port
    # is bound but not yet listening, so a caller's connect never completes; on a CI
    # box whose resolver drops reverse queries for 127.0.0.1 that ran past 30s and
    # read as an unreachable node.
    def server_bind(self):
        socketserver.TCPServer.server_bind(self)
        self.server_name, self.server_port = self.server_address[:2]
t0 = time.time()
srv = Node(('127.0.0.1', 8599), H)
host, port = srv.server_address[:2]
print('listening on %s:%d after %.1fs' % (host, port, time.time() - t0), flush=True)
srv.serve_forever()
```

### 3.2 Start the mock node

```bash
python3 mock_node.py &
```

### 3.3 Write the readiness check

```
import json, socket, sys, time, urllib.request
deadline = time.time() + 30
while time.time() < deadline:
    try:
        socket.create_connection(("127.0.0.1", 8599), timeout=1).close()
        break
    except OSError:
        time.sleep(0.2)
else:
    try: log = open("mock.log").read().strip() or "(empty)"
    except OSError: log = "(no mock.log)"
    sys.exit("nothing accepted on 127.0.0.1:8599 after 30s; mock.log: " + log)
req = urllib.request.Request(
    "http://127.0.0.1:8599",
    data=json.dumps({"jsonrpc": "2.0", "id": 1,
                     "method": "eth_chainId", "params": []}).encode(),
    headers={"content-type": "application/json"})
print("mock node answered:", urllib.request.urlopen(req, timeout=5).read().decode())
```

### 3.4 Wait until the node answers

`python3 mock_node.py &` returns once the shell has forked, not once the listener is
up, so this waits for a connection rather than guessing at a sleep. Answering
`eth_chainId` here also separates the fixture from the module: if this passes and the
round-trip below fails, the node was reachable and the module's transport is what did
not reach it.

```bash
python3 wait_for_node.py
```

---

## Step 4: Run the daemon and drive the client

### 4.1 Write the chain configs

Two chains: chain 1 points at the mock node with no proxy required;
chain 9 **requires** a proxy but is given none — so it must fail closed.

```json
{ "endpoint": "http://127.0.0.1:8599", "proxyRequired": false }
```

### 4.2 Write the fail-closed chain config

```json
{ "endpoint": "http://127.0.0.1:8599", "proxyRequired": true }
```

### 4.3 Start the daemon

```bash
logoscore -D -m ./modules > logs.txt &
```

```bash
sleep 3
```

### 4.4 Load the module

```bash
./logos/bin/logoscore load-module eth_rpc_module
```

### 4.5 Configure chain 1 (mock node, no proxy)

```bash
logoscore call eth_rpc_module set_chain_config 1 @chain_ok.json
```

### 4.6 List configured chains

```bash
./logos/bin/logoscore call eth_rpc_module list_chains
```

### 4.7 Verify the chain ID (real RPC round-trip)

`verify_chain_id` issues a live `eth_chainId` to the mock node.

```bash
logoscore call eth_rpc_module verify_chain_id 1
```

### 4.8 Read a balance (round-trip)

```bash
logoscore call eth_rpc_module get_balance 1 <address>
```

### 4.9 Configure chain 9 (proxy REQUIRED, none available)

```bash
./logos/bin/logoscore call eth_rpc_module set_chain_config 9 @chain_fc.json
```

### 4.10 Fail-closed: the request is refused

Chain 9 requires a proxy but none is configured, so the module refuses
to send the request in the clear — the wallet's privacy guarantee.

```bash
logoscore call eth_rpc_module get_balance 9 <address>
```

### 4.11 Stop the daemon and the mock node

```bash
trap '' TERM
./logos/bin/logoscore stop || true
pkill -f mock_node.py 2>/dev/null || true
true

```

```bash
sleep 2
```

### 4.12 Confirm the daemon has stopped

```bash
./logos/bin/logoscore status || true
```
