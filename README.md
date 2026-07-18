# putioarr

Proxy that allows put.io to be used as a download client for sonarr/radarr/whisparr. The proxy uses the Transmission protocol.

## Installation

There are a few ways to install putioarr:

### Cargo
Make sure you have a [proper rust installation](https://www.rust-lang.org/tools/install)
`cargo install putioarr`

#### Usage

First, generate a config using `putio generate-config`. This will generate a config file in `~/.config/putioarr/config.toml`. Use `-c` to override the configuration file location.

Edit the configuration file and make sure you configure the username and password, as well as the sonarr/radarr/whisparr details.

- Run the proxy:`putioarr run`
- Configure the Transmission download client in sonarr/radarr/whisparr:
    - Url Base: /transmission
    - Username: <configured username>
    - Password: <configured password>


### Docker

Docker images are based on [linuxserver.io](https://linuxserver.io) images.

#### Usage

The first time you run your docker container, run it without the `-d` option, since you'll need a put.io API key. When no configuration is found, it will present you a link and a code that will generate an API key. After the key is generated, putioarr will write a default config in your config volume (see `docker compose` and `docker cli` below). Modify the config (like username, password and sonarr/radarr/whisparr configuration) in order to properly use putioarr.

#### Supported Architectures

We utilise the docker manifest for multi-platform awareness.

Simply pulling `ghcr.io/wouterdebie/putioarr:latest` should retrieve the correct image for your arch (amd64 or arm64).

#### docker-compose (recommended, [click here for more info](https://docs.linuxserver.io/general/docker-compose))
```yaml
---
version: "2.1"
services:
  putioarr:
    image: ghcr.io/wouterdebie/putioarr:latest
    container_name: putioarr
    environment:
      - PUID=1000
      - PGID=1000
      - TZ=Etc/UTC
    volumes:
      - /path/to/putioarr/config:/config
      - /path/to/your/downloads:/downloads
    ports:
      - 9091:9091
    restart: unless-stopped
```

#### docker cli ([click here for more info](https://docs.docker.com/engine/reference/commandline/cli/))

```bash
docker run -d \
  --name=putioarr \
  -e PUID=1000 \
  -e PGID=1000 \
  -e TZ=Etc/UTC \
  -p 9091:9091 \
  -v /path/to/putioarr/config:/config \
  -v /path/to/your/downloads:/downloads \
  --restart unless-stopped \
  ghcr.io/wouterdebie/putioarr:latest

```
#### Parameters

Container images are configured using parameters passed at runtime (such as those above). These parameters are separated by a colon and indicate `<external>:<internal>` respectively. For example, `-p 8080:80` would expose port `80` from inside the container to be accessible from the host's IP on port `8080` outside the container.

| Parameter | Function |
| :----: | --- |
| `-p 9091` | Port connecting to putioarr |
| `-e PUID=1000` | for UserID - see below for explanation |
| `-e PGID=1000` | for GroupID - see below for explanation |
| `-e TZ=Etc/UTC` | specify a timezone to use, see this [list](https://en.wikipedia.org/wiki/List_of_tz_database_time_zones#List). |
| `-v /config` | putioarr configs |
| `-v /downloads` | torrent download directory |



## Behavior
The proxy will upload torrents or magnet links to put.io. It will then continue to monitor transfers. When a transfer is completed, all files belonging to the transfer will be downloaded to the specified download directory. The proxy will remove the files after sonarr/radarr/whisparr has imported them and put.io is done seeding. The proxy will skip directories named "Sample".

## Configuration
A configuration file can be specified using `-c`, but the default configuration file location is:
- Linux: ~/.config/putioarr/config.toml
- MacOS: ~/Library/Application Support/nl.evenflow.putioarr

TOML is used as the configuration format:
```
# Required. Username and password that sonarr/radarr/whisparr use to connect to the proxy
username = "myusername"
password = "mypassword"

# Required. Directory where the proxy will download files to. This directory has to be readable by
# sonarr/radarr/whisparr in order to import downloads
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

# Optional number of orchestration workers, default 10. Unless there are many changes coming from
# put.io, you shouldn't have to touch this number. 10 is already overkill.
orchestration_workers = 10

# Optional number of download workers, default 4. This controls how many downloads we run in parallel.
download_workers = 4

[putio]
# Required. Putio API key. You can generate one using `putioarr get-token`
api_key =  "MYPUTIOKEY"

# Both [sonarr] and [radarr] are optional, but you'll need at least one of them
[sonarr]
url = "http://mysonarrhost:8989/sonarr"
# Can be found in Settings -> General
api_key = "MYSONARRAPIKEY"
# Optional: Download to a category-specific subdirectory (e.g., /downloads/tv/)
# category = "tv"

[radarr]
url = "http://myradarrhost:7878/radarr"
# Can be found in Settings -> General
api_key = "MYRADARRAPIKEY"
# Optional: Download to a category-specific subdirectory (e.g., /downloads/movies/)
# category = "movies"

[whisparr]
url = "http://mywhisparrhost:6969/whisparr"
# Can be found in Settings -> General
api_key = "MYWHISPARRAPIKEY"
# Optional: Download to a category-specific subdirectory (e.g., /downloads/adult/)
# category = "adult"
```

### Category-based Download Directories

To prevent Sonarr/Radarr/Whisparr from seeing each other's downloads, you can configure separate download directories using categories:

1. **In putioarr's config.toml**, add a `category` field to each service:
   ```toml
   [sonarr]
   category = "tv"
   
   [radarr]
   category = "movies"
   ```

2. **In Sonarr/Radarr/Whisparr's download client settings**, set the same category in the Transmission configuration.

3. Downloads will then be organized into subdirectories:
   - Sonarr downloads → `/download_directory/tv/`
   - Radarr downloads → `/download_directory/movies/`
   - Whisparr downloads → `/download_directory/adult/`

This feature ensures each *arr application only sees and processes its own downloads.

## Connecting Sonarr/Radarr/Whisparr

putioarr pretends to be a Transmission client, so you add it in the *arr just
like a real Transmission instance.

In **Settings → Download Clients → Add → Transmission**, set:

| Field | Value |
| --- | --- |
| Host | the host running putioarr (e.g. `putioarr` if on the same Docker network, or its IP) |
| Port | `9091` (or your configured `port`) |
| URL Base | `/transmission` |
| Username | the `username` from your config |
| Password | the `password` from your config |
| Category | the category for this *arr, if you set one (e.g. `tv` for Sonarr, `movies` for Radarr) — must match the `category` in putioarr's config |

Click **Test**; it should turn green. Then **Save**.

Repeat for each *arr, using that *arr's category.

> **Important — shared download path.** The *arr imports a completed download by
> reading the files from disk itself, so it must be able to see the files
> putioarr downloaded, **at the same path**. In other words
> `download_directory` in putioarr and the path the *arr looks in must resolve
> to the same files. The easiest way is to mount the *same* host directory into
> both containers at the same path (e.g. `/downloads` in putioarr and
> `/downloads` in Sonarr/Radarr). If the paths differ, add a **Remote Path
> Mapping** in the *arr (Settings → Download Clients → Remote Path Mappings) so
> it can translate putioarr's path to where the *arr sees it. Without this the
> download completes and is cleaned up, but nothing is ever imported.

## Troubleshooting

**putioarr downloads (and deletes) the files, but nothing is imported into
Sonarr/Radarr.**
This is almost always a path problem: the *arr can't find the files putioarr
downloaded because it's looking at a different path (or doesn't have the volume
mounted at all). Make sure the download directory is mounted into both
containers at the same path, or configure a Remote Path Mapping — see *Connecting
Sonarr/Radarr/Whisparr* above. Confirm the *arr can actually list the files at
`download_directory` (or its mapped equivalent).

**The download client tests fine but grabs never appear on put.io.**
Check that the *arr is sending grabs to putioarr (the Activity → Queue in the
*arr) and watch putioarr's logs — every grab logs an "uploaded" line. Make sure
the `[putio] api_key` is valid (regenerate with `putioarr get-token`).

**Downloads go to the wrong folder / one *arr imports another's downloads.**
Set a distinct `category` per *arr in putioarr's config *and* the matching
Category in each *arr's Transmission settings. See *Category-based Download
Directories* above.

**Nothing happens / no logs.** Raise the log level (`loglevel = "debug"` in the
config, or run with `RUST_LOG=debug`) to see what putioarr is doing.

## TODO:
- Better Error handling and retry behavior
- The session ID provided is hard coded. Not sure if it matters.
- (Add option to not delete downloads)
- Figure out a better way to map a transfer to a completed import. Since a transfer can contain multiple files (e.g. a whole season) we currently check if all video files have been imported. Most of the time this is fine, except when there are sample videos. sonarr/radarr/whisparr will not import samples, but will make no mention of the fact that the sample was skipped. Right now we check against the `skip_directories` list, which works, but might be tedious.
- Automatically pick the right putio proxy based on speed

## Thanks
Thanks to [davidchalifoux](https://github.com/davidchalifoux) for borrowed code from kaput-cli.
