# Digital Photo Frame

This project is intended to run on digital photo frame running a Raspberry Pi. It features a slideshow to display photos.

Main features:

- connect to an Immich API to load the photos
- the slide transition should be a fade
- if the user taps on the touchscreen display (or click on the left/right side of the image) it should go forward or backward)

- settings (persisted, not only in-memory):
  - define time for each photo slide
  - Immich URL and API key
  - Poll frequency (5 min by default)

The idea is to download the images from the API and save into the frame SD card. The screen is 1024x600, so the download should transform the images taking that into account. To avoid having stale images, we should clean up from time to time to match the IDs that are still available on Immich.

## Next steps

Update software from git
