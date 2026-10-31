# Yappr

Your machines post to you. Some of them wait for your answer.

One machine writes a private, append-only feed. Your other computers read it
over iroh, from anywhere. A script can ask a question and wait until you
answer on another computer. There is no account and no hosted service.

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

On the server, start the feed and create a pairing code:

```sh
server$ yappr serve
server$ yappr pair --alias laptop      # in a second terminal
```

On the laptop, join with that code:

```sh
laptop$ yappr join 'yappr://pair/...' --name laptop --alias server
```

Compare the laptop's device ID with the ID on the server. Then enter `y` on
the server. Now the laptop can follow the feed:

```sh
laptop$ yappr follow server
server$ echo "Backup complete" | yappr post --source backup
```

## Examples

### Stream a log

```sh
journalctl -f -u nginx | yappr post --lines --source nginx
tail -F /var/log/app.log | yappr post --every 5s --source app
```

`--lines` posts each line. `--every 5s` collects the lines of each 5 seconds
into one entry. These entries have the kind `log`. To read a feed without its
log streams, use `yappr follow server --not-kind log`.

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

Find the question ID in the JSON output, then answer:

```sh
laptop$ yappr follow server --no-follow --output json \
  | jq -r 'select(.kind == "question") | "\(.choices.question.id)  \(.title)"'
laptop$ yappr answer server <QUESTION_ID> --choice approve
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
yappr devices          # aliases, IDs, and which devices can read this feed
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
