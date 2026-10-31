# Yappr

Yappr sends you updates and questions from your own machines.

A program on one machine posts entries to a feed, and your other devices read
them. A script can also ask you a question and wait until you answer from
another device. The feed keeps every entry, so a device that was offline
catches up when it connects again.

Yappr connects your devices directly with iroh. When a direct connection is
not possible, for example behind a strict firewall, the traffic goes through a
relay. The relay passes on encrypted data and cannot read it. You do not need
an account, and no Yappr server sits between your devices.

```sh
df -h / | yappr post --source disk --title "Disk usage"
yappr approve laptop "Deploy to production?" && ./deploy
```

## Install

```sh
cargo install --git https://github.com/fractalops/yappr
```

## Quick start

In this example, `server` writes the feed and `laptop` reads it.

On the server, start the feed and pair your first device:

```sh
yappr init
```

The server shows a QR code and a pairing code that starts with `YAPPR:`. On a
phone, scan the QR code with the browser app at [yappr.run](https://yappr.run).
On the laptop, join with the pairing code:

```sh
yappr join YAPPR:... --name laptop --alias server
```

The server shows the name and the ID of the new device. Check that the ID
matches the ID on the device, then enter `y`. The server saves an alias from
the device name, for example `laptop`, and keeps serving. Next time, start the
server with `yappr serve`. To pair more devices, run `yappr pair` in a second
terminal.

On the laptop, follow the feed:

```sh
yappr follow server
```

On the server, post an entry. It appears on the laptop:

```sh
echo "Backup complete" | yappr post --source backup
```

## Examples

### Post a command's output

```sh
df -h / | yappr post --source disk --title "Disk usage"
uptime | yappr post --source server --kind warn --title "Load is high"
```

`post` reads all of stdin and adds it as one entry. The feed keeps every
entry, so post events, not a live log stream.

### Ask before a deploy

```sh
if yappr approve laptop "Deploy to production?" --body "Commit abc123" --timeout 10m
then
  ./deploy abc123
fi
```

Approval exits with 0. Rejection exits with 7. Timeout and cancellation have
their own exit codes. Yappr does not do the action. Your script does.

### Let a script choose the next step

```sh
case $(yappr ask laptop "Disk is full. What next?" --choice clean --choice stop) in
  clean) ./cleanup ;;
  stop)  exit 1 ;;
esac
```

`ask` prints the value of the selected choice.

### Answer a question

On the laptop, find the question ID in the JSON output:

```sh
yappr follow server --no-follow --output json \
  | jq -r 'select(.kind == "question") | "\(.choices.question.id)  \(.title)"'
```

Then answer it:

```sh
yappr answer server <QUESTION_ID> --choice approve
```

If the server is not reachable, the answer waits in a local outbox.
`yappr retry` sends it again.

### Follow several machines

```sh
yappr follow server nas
yappr follow server nas --output json
```

`follow` prints the entries that it missed, then waits for new entries. After
a connection failure or sleep, it continues from where it stopped.

## Devices and access

```sh
yappr status           # this machine, its peers, and open questions
yappr allow laptop
yappr deny laptop
yappr health server    # connection state, path, and round-trip time
```

A ticket is an address, not a secret. The writer denies unknown devices by
default. Read access does not let a device answer a question. Each question
names the one device that can answer it. When you deny a device, a running
`serve` closes its feed within five seconds.

## More

The [scripting reference](docs/scripting.md) describes the entry format, exit
codes, questions, pairing, several writers, relays, and storage.

## License

MIT OR Apache-2.0
