# gangli's local API (for programs and AI agents)

While gangli is open on a computer (the window, the tray, or `gangli console run`), its console answers on
`http://127.0.0.1:7373/v1` (the port is `[console] port` in `gangli.console`). Only that computer can reach it.

## Turning it on, giving it to an agent

1. In the window: **API** → *Enabled*, then make a token. `admin` lets the token do everything the window and
   the command line do (list and identify the machines, run commands, connect over SSH, make tunnels, manage
   networks), except the API itself and its tokens. `exec` stops at commands, jobs and terminals, `read` at
   looking.
2. **Copy instructions for an AI agent** puts on the clipboard a short text with the address, the token and the
   main calls: paste it into the agent's prompt, or into a file it reads. From a terminal the same text goes
   into a file readable by you only:

   ```
   gangli api enable
   gangli api token add claude --scope admin --save gangli-agent.txt
   ```

3. Revoke it whenever you like (API → *revoke*, or `gangli api token remove claude`).

An agent that runs on the same computer as gangli and can use its command line needs no token at all:
`gangli <command> --json` (`gangli --help`) does the same as the API.

## Calls

Every request carries `Authorization: Bearer <token>`. Answers are `{"ok": true, "data": …}` or
`{"ok": false, "error": "…"}` (401: no or unknown token, 403: the token's scope does not allow it).

- `GET /v1/help`: every operation with its parameters and scope (start here).
- `POST /v1/op` with a JSON body `{"op": "<name>", …parameters}`, for example:

  ```
  {"op": "nodes"}
  {"op": "exec", "node": "nas", "command": "df -h", "timeout": 60}
  {"op": "exec", "node": "nas", "user": "system", "command": "systemctl restart smbd"}
  {"op": "tunnel_add", "name": "nas-web", "node": "nas", "target": "192.168.1.10:80", "listen": "8080"}
  {"op": "ssh_login_add", "hosts": ["*"], "users": ["*"], "key": "ssh-ed25519 AAAA… agent"}
  {"op": "network_relays", "name": "family", "set": ["https://relay.example.org"]}
  ```

  `network_relays` without `set` shows a network's extra relays (read scope); with `set` it replaces them
  (`[]`: the n0 relays only) and sends them to the network's machines (admin scope).

- `GET /v1/status`, `GET /v1/nodes`, `GET /v1/nodes/<name>`: shortcuts for `status`, `nodes`, `info`.

**The state of each machine** is in `nodes` / `info`: `online`, `since` (this session) or `last_seen`, and
while online `path`: `relay` (true: through a relay, slower; false: direct), `rtt_ms`, `remote`, and the
traffic of the whole connection, `rx_bps` / `tx_bps` (bytes per second, received / sent). A machine that goes
away comes back by itself (its node knocks); check `online` before acting, and retry an `exec` that failed
because it went offline. A background job (`job_start`) keeps running on the machine while the console is
away; it ends if the machine itself restarts.

A machine is its name, or `name.network` when two networks have one with that name. `user` is an account on
the machine, `system` (root / SYSTEM) or `desktop` (whoever is at the screen); without it, the desktop user
when there is one, else the service account.

## SSH with any client

The console also runs an SSH gateway on `127.0.0.1:2222`: the login is `user+machine` (`root+nas`), and
`user+machine+name` opens the persistent terminal `name`. An agent adds its public key once
(`ssh_login_add`, as above), then uses any client: `ssh -p 2222 root+nas@127.0.0.1 uptime`, `scp -P 2222 …`,
`ssh -L 8080:192.168.1.10:80 …`. `{"op": "ssh_password", "node": "nas"}` gives a password instead.

The SSH client talks to the local gateway, so it never sees a machine go away. An interactive shell then
waits: the gateway writes `[gangli: the machine is not reachable: this session goes on when it is back]` and,
when it is, goes on in the same session (a persistent terminal reattached with its screen, else a new shell,
said in a dim line). A command (`ssh … uptime`) fails instead, with an error saying it may not have
finished. To find a screen again after closing the SSH client itself, use a persistent terminal
(`user+machine+name`).

For people: put your key in a rule for `*` machines and `*` users (SSH page of the window, or
`gangli ssh login add --key "ssh-ed25519 …"`), and PuTTY / WinSCP opened from the window log in with Pageant,
no password.
