# Security

## Reporting a vulnerability

Report privately through [GitHub's advisory
form](https://github.com/AcidDemon/plugkill/security/advisories/new) rather
than in a public issue. Expect an acknowledgement within a week.

If you would rather write, the address is acid@acidnetworks.net, and the key
is the one that signs the commits in this repository:

    789A D61C 5636 3D3B 7F74  56E6 D90E F882 774B D645

## What counts

plugkill powers machines off. The failures worth reporting are the ones that
change whether it does that, or let someone else decide:

- A way for a user who is not root and not in the socket group to reach the
  control socket, or to make the daemon act on anything they send.
- A way past the polkit gate with `require_auth` on: disarm, learn, reload,
  `--pair` or `--allow-last` going through without the prompt being answered.
- A device change that should be a violation and is not. A whitelist or
  allowance matching more than it names, a baseline capture that quietly
  accepts what is plugged in, a bus that stops being checked without saying so.
- A kill that fires when nothing was violated, or a destruction step that
  touches a path the config did not name.
- Anything in the relay that lets an unsigned or replayed message fire a kill
  on a peer.

## What does not

These are documented behaviour rather than bugs, and the README says so:

- plugkill does not defend against physical access while the machine is
  unlocked. `require_auth` raises the cost, but someone who knows your
  password still gets in, and nothing stops them pulling the plug.
- Learning mode is not a safe audit mode inside a relay mesh: the daemon
  refuses a peer's kill and the relay powers its own machine off in response.
- On FreeBSD, `require_auth` refuses every non-root caller, because polkit is
  compiled Linux-only and the peer-credentials sockopt there carries no pid.
- A config file that root can write is trusted. Anyone who can edit it can
  already choose what root executes.

## Supported versions

While the major version is 0, only the latest release is supported.
