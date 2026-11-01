<h1><img src="docs/assets/yappr.svg" alt="" width="48" height="48" align="middle" /> Yappr</h1>

**Give your scripts and agents a way to reach you and get an answer.**

Yappr brings updates, questions, and approval requests from your machines to
your other devices. A script can wait for your reply before it continues. Each
machine keeps its feed, so devices that were offline catch up when they
reconnect.

Your devices connect directly over iroh where possible. Otherwise, an iroh
relay server forwards their end-to-end encrypted traffic. The relay cannot read
your entries or answers, and it does not store your feed. You need no account
or hosted feed service.

```sh
df -h / | yappr post --source disk --title "Disk usage"
yappr approve laptop "Deploy to production?" && ./deploy
```

## Try it

Open [yappr.run](https://yappr.run/) on your phone or computer to read feeds
and answer questions. After installing the CLI on your computer, run
[`yappr init`](#start-with-yappr-init) to pair the browser app.

## Install

```sh
cargo install --git https://github.com/fractalops/yappr
```

## Start with `yappr init`

Start the feed and pair your first device:

```sh
yappr init
```

<img src="docs/assets/init.gif" alt="yappr init shows a pairing code, accepts a laptop, and keeps serving the feed" width="800" />

`init` starts the feed and shows QR and `YAPPR:` pairing codes, valid for five minutes.

Scan the QR code at [yappr.run](https://yappr.run) on your phone, or join from a laptop:

```sh
yappr id
yappr join YAPPR:... --name laptop --alias server
```

Replace `YAPPR:...` with the code from the server.
Check that the device ID on the server matches the ID on the joining device.
Enter `y` on the server to grant access.
The server saves the device name as an alias, such as `laptop`, and keeps serving.

On the laptop, read the feed and wait for new entries:

```sh
yappr read server -f
```

In a second terminal on the server, post an entry. It appears on the laptop:

```sh
echo "Backup complete" | yappr post --source backup
```

Next time, use `yappr serve` to start the server.
To pair another device while it runs, use `yappr pair` in a second terminal.

## Examples

### Post a command's output

```sh
df -h / | yappr post --source disk --title "Disk usage"
uptime | yappr post --source server --title "Uptime"
```

`post` reads stdin until it ends, then saves the output as one entry.
Use it for a command's output or an individual event.

### Ask before a deploy

```sh
if yappr approve laptop "Deploy to production?" --body "Commit abc123" --timeout 10m
then
  ./deploy abc123
fi
```

`approve` exits with 0 for approval, 7 for rejection, 6 for expiry, or 8 for
cancellation. The script runs `deploy` after approval. All commands share the
same [exit codes](docs/reference.md#exit-codes).

### Choose what a script does next

```sh
choice=$(yappr ask laptop "Disk is full. What next?" \
  --choice clean --choice stop) || exit $?

case "$choice" in
  clean) ./cleanup ;;
  stop)  exit 1 ;;
esac
```

`ask` prints the selected value, such as `clean`.
The script stops on expiry, cancellation, or error.

### Answer a question

In the browser app, select a choice on the question.
On the laptop, use the CLI to find the question ID:

```sh
yappr read server --since 0 --output json \
  | jq -r 'select(.kind == "question") | "\(.choices.question.id)  \(.title)"'
```

Replace `QUESTION_ID` with an ID from the output. For an approval, answer with:

```sh
yappr answer server QUESTION_ID approve
```

Yappr saves your answer locally before sending it.
If the server is offline, run `yappr retry` when it is reachable again.

### Read several machines

```sh
yappr read server nas
yappr read server nas -f --output json
```

Each machine has its own saved position. `read` prints the entries that you
have not read yet. With `-f`, it then waits for new ones, and it resumes after
sleep or a connection failure.

## Manage devices

```sh
yappr status           # this machine, its devices, questions, and queued answers
yappr allow laptop
yappr deny laptop
yappr health server    # connection state, path, and round-trip time
```

Pairing or `yappr allow` grants read access. Only the assigned device can answer
each question. `yappr deny` revokes access and closes active feed connections
within five seconds.

## Reference

The [CLI reference](docs/reference.md) describes the entry format, exit
codes, questions, pairing, several writers, relays, and storage.

## License

MIT OR Apache-2.0
