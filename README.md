# putioarr

Proxy that allows put.io to be used as a download client for Sonarr/Radarr. The proxy uses the Transmission protocol.

## Installation
Make sure you have a [proper rust installation](https://www.rust-lang.org/tools/install)
`cargo install putioarr`

## Usage

- Get a put.io API key: `putioarr get-token`
- Create a config file (see below)
- Run the proxy:`putioarr run`
- Configure the Transmission download client in Sonarr/Radarr:
    - Url Base: /transmission
    - Username: <configured username>
    - Password: <configured password>
    - Directory: <sonarr/radarr download dir>

## Behavior
The proxy will upload torrents or magnet links to put.io. It will then continue to monitor transfers. When a transfer is completed, all files belonging to the transfer will be downloaded to the specified download directory. The proxy will remove the files after Sonarr/Radarr has imported them and put.io is done seeding. The proxy will skip directories named "Sample".

## Configuration
A configuration file can be specified using `-c`, but the default configuration file location is:
- Linux: ~/.config/putioarr/config.toml
- MacOS: ~/Library/Application Support/nl.evenflow.putioarr

TOML is used as the configuration format:
```
username = "myusername"
password = "mypassword"
download_directory = "/path/to/downloads"

# Optional bind address, default "0.0.0.0"
bind_address = "0.0.0.0"

# Optional TCP port, default 9091
port = 9091

# Optional log level, default "info"
loglevel = "info"

# Optional UID, default 1000. Change the owner of the downloaded files to this UID. Requires root.
uid = 1000

# Optional polling interval in secs, default 10.
polling_interval = 10

# Optional skip directories when downloding, default ["sample", "extras"]
skip_directories = ["sample", "extras"]

[putio]
api_key =  "MYPUTIOKEY"

# Both [sonarr] and [radarr] are optional, but you'll need at least one of them
[sonarr]
url = "http://mysonarrhost:8989/sonarr"
# Can be found in Settings -> General
api_key = "MYSONARRAPIKEY"

[radarr]
url = "http://myradarrhost:7878/radarr"
# Can be found in Settings -> General
api_key = "MYRADARRAPIKEY"
```

## TODO:
- Better Error handling and retry behavior
- Multi-threaded downloads (?)
- The session ID provided is hard coded. Not sure if it matters.
- (Add option to not delete downloads)
- Docker image
- Figure out a better way to map a transfer to a completed import. Since a transfer can contain multiple files (e.g. a whole season) we currently check if all video files have been imported. Most of the time this is fine, except when there are sample videos. Sonarr/radarr will not import samples, but will make no mention of the fact that the sample was skipped. Right now we check against the `skip_directories` list, which works, but might be tedious.
- Automatically pick the right putio proxy based on speed

## Thanks
Thanks to [davidchalifoux](https://github.com/davidchalifoux) for borrowed code from kaput-cli.
