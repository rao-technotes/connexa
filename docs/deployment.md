# Deployment

Connexa's server is one binary (`signaling-server`, container image
`ghcr.io/rao-technotes/connexa-signaling`). Features switch on through
configuration ([networking.md](networking.md#server-configuration)):

| you add | you get |
|---|---|
| nothing | signaling, web client, in-memory device trust (single node) |
| `CONNEXA_DATABASE_URL` (Postgres) | persistent device identities, trusted devices, audit log |
| `CONNEXA_SFU=true` + UDP port | large meetings (up to 25) |
| `CONNEXA_REDIS_URL` + slots | several signaling nodes behind a load balancer |
| `CONNEXA_METRICS_TOKEN` | protected Prometheus metrics at `/metrics` |

## One host (Docker Compose)

`infrastructure/deployment` runs Caddy (automatic HTTPS), the server with the
SFU, Postgres and coturn:

```bash
cd infrastructure/deployment
cp .env.example .env        # domain, public IP, secrets
docker compose up -d --build
```

Open these ports on the host: 80/443 tcp, 3478 tcp+udp (STUN/TURN), 3479 udp
(SFU) and 49160–49200 udp (TURN relays).

### Monitoring

```bash
docker compose --profile monitoring up -d
```

This adds Prometheus (scraping `/metrics` with the bearer token) and Grafana
at `127.0.0.1:3000`, provisioned with the **Connexa** dashboard. The
dashboard shows rooms, participants, connections, joins, errors by code, abuse
signals (rate limiting, wrong PINs, lobby denials) and SFU peers, tracks and
packet rates. Reach Grafana over an SSH tunnel:
`ssh -L 3000:localhost:3000 your-host`.

Main metrics:

| metric | type | meaning |
|---|---|---|
| `connexa_rooms`, `connexa_sfu_rooms` | gauge | active rooms |
| `connexa_participants`, `connexa_connections` | gauge | people in rooms, open sockets |
| `connexa_lobby_waiting` | gauge | joiners waiting for a host |
| `connexa_joins_total`, `connexa_rooms_created_total`, `connexa_resumes_total` | counter | activity |
| `connexa_errors_total{code}` | counter | errors returned to clients |
| `connexa_rate_limited_total`, `connexa_pin_failures_total`, `connexa_denied_total` | counter | abuse signals |
| `connexa_sfu_peers`, `connexa_sfu_tracks`, `connexa_sfu_packets_forwarded` | gauge | SFU load |
| `connexa_info{version,store,node_slot,sfu}` | gauge | build and configuration |

## Kubernetes

`infrastructure/kubernetes` is a kustomize base: a 3-node signaling
StatefulSet, Redis, Postgres, an Ingress and a PodDisruptionBudget.

```bash
cp infrastructure/kubernetes/secrets.example.yaml secrets.yaml   # fill in
kubectl create namespace connexa
kubectl -n connexa apply -f secrets.yaml
# set your domain and TURN host in signaling.yaml / ingress.yaml, then:
kubectl apply -k infrastructure/kubernetes
```

- **Scaling.** Each pod's slot comes from its ordinal (`connexa-signaling-0`
  gets slot 1), up to 9 replicas. The Service and Ingress round-robin with no
  sticky sessions. Scaling down ends the rooms owned by the removed pods, and
  their clients are told `server_lost`.
- **SFU.** Pods expose UDP 3479 with `hostPort` (one pod per node, enforced by
  anti-affinity) and advertise the node's IP. On clusters with private node
  IPs, give nodes public IPs or run the SFU on VMs and set
  `CONNEXA_SFU_PUBLIC_IP` accordingly.
- **Stateful pieces.** Redis only carries messages between nodes (no
  persistence needed). Use managed Postgres and Redis for high availability.
- **TURN.** Run coturn on hosts with public IPs, outside the cluster, and
  point `CONNEXA_TURN_URLS` at them. It shares `turn-secret` with the server.
- **Metrics.** The Service carries `prometheus.io/*` annotations. Configure
  your Prometheus to send `Authorization: Bearer <metrics-token>`. The
  Ingress hides `/metrics` from the Internet.

## Releases

Pushing a `v*` tag makes CI publish the server image to GHCR, and a GitHub
Release with the Windows installer, Android APK and Chrome extension.
