# Nucleus HTTP API v1

Base URL: `http(s)://<panel>/api/v1`

Authenticate every request with an API key (Account → API keys):

```
Authorization: Bearer nuc_XXXXXXXXXXXXXXXXXXXXXXXX
```

Permissions mirror the web UI's per-server subuser flags. Errors are always
`{"error": "..."}` with 401 (bad key), 403 (no permission), 404 (unknown
server), or 502 (node error).

## Servers

| Method | Path | Perm | Description |
|---|---|---|---|
| GET | `/servers` | any | Servers visible to the key's user, with running state |
| GET | `/servers/{id}` | console | Full status (`running`, `exit_code`) |
| GET | `/servers/{id}/stats` | console | Live CPU / memory / network |
| GET | `/servers/{id}/logs?tail=N` | console | Last N console lines |
| POST | `/servers/{id}/power` | power | `{"action": "start\|stop\|restart\|kill"}` |
| POST | `/servers/{id}/config` | settings | Patch config (subset of the fields below) |
| POST | `/servers` | **admin** | Create a server (body below) |
| DELETE | `/servers/{id}?purge_data=true` | **admin** | Delete server (and data) |

`POST /servers` body (`backup_quiesce` is a boolean or `null` for auto):

```json
{
  "node_id": "6e698af66b",
  "id": "optionalCustomId",
  "name": "My Server",
  "image": "ghcr.io/pterodactyl/yolks:java_17",
  "startup": "java -jar server.jar",
  "env": {"KEY": "value"},
  "ports": [{"host": 25565, "container": 25565, "proto": "tcp"}],
  "limits": {"mem_mb": 2048, "cpu_cores": 2.0, "disk_mb": 0, "pids_limit": 0},
  "stop_command": "stop",
  "accept_eula": true,
  "backup_retention": 7,
  "backup_quiesce": null,
  "tags": "minecraft"
}
```

## Files

| Method | Path | Perm | Description |
|---|---|---|---|
| GET | `/servers/{id}/files?path=/` | files | List entries (JSON) |
| GET | `/servers/{id}/files/content?path=/x` | files | Raw contents (octet-stream) |
| PUT | `/servers/{id}/files/content?path=/x` | files | Write raw body to path |
| POST | `/servers/{id}/files/mkdir` | files | `{"path": "/dir"}` |
| POST | `/servers/{id}/files/delete` | files | `{"path": "/x"}` (file or dir) |
| POST | `/servers/{id}/files/rename` | files | `{"from": "/a", "to": "/b"}` |

```sh
curl -H "Authorization: Bearer $KEY" "$BASE/servers/$SID/files/content?path=/server.properties"
echo "motd=Hi" | curl -X PUT -H "Authorization: Bearer $KEY" --data-binary @- \
  "$BASE/servers/$SID/files/content?path=/server.properties"
```

## Backups

| Method | Path | Perm | Description |
|---|---|---|---|
| GET | `/servers/{id}/backups` | backups | List (`id`, `size`, `created_at`) |
| POST | `/servers/{id}/backups` | backups | Create (runs world-save quiesce per policy; retention prunes) |
| GET | `/servers/{id}/backups/{bid}/download` | backups | Stream archive (application/gzip) |
| DELETE | `/servers/{id}/backups/{bid}` | backups | Delete archive |
| POST | `/servers/{id}/backups/{bid}/restore` | backups | Stop, wipe data dir, extract |

Creating and restoring can take minutes on large servers — both are
synchronous.

## Schedules

| Method | Path | Perm | Description |
|---|---|---|---|
| GET | `/servers/{id}/schedules` | schedules | List tasks with `next_run` |
| POST | `/servers/{id}/schedules` | schedules | `{"name","cron","action","payload"}` |
| PUT | `/servers/{id}/schedules/{tid}` | schedules | `{"enabled": false}` |
| DELETE | `/servers/{id}/schedules/{tid}` | schedules | Remove task |
| POST | `/servers/{id}/schedules/{tid}/run` | schedules | Run now (synchronous) |

`cron` is a 5-field crontab. `action` is `command` (payload = console line),
`power` (payload = start|stop|restart|kill), or `backup` (payload unused).
`run` blocks until the action finishes; backups can take minutes.

## Example

```sh
KEY=nuc_...
BASE=http://panel.example.com/api/v1
SID=$(curl -s -H "Authorization: Bearer $KEY" $BASE/servers | \
      python3 -c 'import json,sys; print(json.load(sys.stdin)[0]["id"])')
curl -s -H "Authorization: Bearer $KEY" $BASE/servers/$SID/stats
```
