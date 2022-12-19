# Video recording and streaming system
This software captures video streams from different video sources, saves them to a local disk recorded video streams.

## videosrv
`cargo build --bin videosrv`

this process makes video capture, recording and live streaming

## Building
`make build`

## Config
`video.yaml` 

To add a new Rotoclear camera, in the `video.yaml` add its IP address under the "sources" add its name and ip address under the "sources" key in video.yaml. See an example [here](./video.yaml)


## API
* curl -v 'http://localhost:8080/recording_list?source=cam1&resolution=240p&timestamp_min=2022-07-01T00:00:00Z&timestamp_max=2022-07-15T23:59:59Z'
* curl -v 'http://localhost:8080/info'
* curl -v 'http://localhost:8080/rec/cam0/240p/221219T000000-221220T000000/playlist.m3u8'
