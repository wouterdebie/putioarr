# putioarr

Proxy that allows put.io to be used as a download client for Sonarr/Radarr. The proxy uses the Transmission protocol.

## Installation

`cargo install putioarr`

## Usage

- Get a put.io API key: `putioarr get-token`
- Run the proxy:`putioarr run -s <file state is kept> -a <putio API key> -d <sonarr/radarr download dir>`
- Configure the Transmission download client in Sonarr/Radarr:
    - Url Base: /transmission
    - Username: anything goes
    - Password: <putio API key>
    - Directory: <sonarr/radarr download dir>
- Make sure Sonarr/Radar uses hardlinks rather than copy

## Behavior:
The proxy will upload torrents or magnet links to put.io. It will then continue to monitor transfers. When a transfer is completed, all files belonging to the transfer will be downloaded to the specified download directory. The proxy will remove the files after Sonarr/Radarr has imported them and put.io is done seeding. A file is determined to be imported if it has more than one hardlink pointed to it.

The default UID that is used to write files is 1000. You can override this with `-u <UID>`


## TODO:
- Better Error handling
- Multi-threaded downloads
- The session ID provided is hard coded. Not sure if it matters.
- Add option to not delete downloads (in case no hardlinks can be used)
- Docker image

## Thanks
Thanks to [davidchalifoux](https://github.com/davidchalifoux) for borrowed code from kaput-cli.
