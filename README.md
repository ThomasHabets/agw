# AGW library

Rust library for speaking AGW with e.g. Direwolf

https://github.com/ThomasHabets/agw

## Example usage

Example setup with ICom 9700

### 1. Run rigctld

```bash
$ rigctld -m 3081 -r /dev/ttyUSB0 -s 19200
```

### 3. Start direwolf

```bash
$ cat > direwolf.conf
ADEVICE pulse
PTT RIG 2 127.0.0.1:4532
CHANNEL 0
MYCALL M0QQQ-8
AGWPORT 8010
KISSPORT 8011
MODEM 1200
^D
$ direwolf -d a -p -t 0 -c direwolf.conf
```

### 4. Run the AGW application

```bash
$ cargo build --example term
$ ./target/debug/examples/term -l blah.log -v 4 M0QQQ-3 GB7CIP
```

## Contributing

Pull requests welcome!

Please enable the pre-commit when developing:

```bash
(cd .git/hooks && ln -s ../../extra/pre-commit)
```

## Links

* [The protocol](https://www.on7lds.net/42/sites/default/files/AGWPEAPI.HTM)
