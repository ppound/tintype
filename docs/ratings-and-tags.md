# Ratings, tags and sidecars

## Ratings

Click the stars in the details panel, or press **1**–**5** with a photo selected
(**0** clears). Search for them with `rating:>=4`, or sort with **Best photos first**.

## Tags

Type in **Add a tag…** in the details panel and press **Enter**. Click a tag to see every
photo with it, and × to remove it from the photo. Tags are listed in the sidebar too, and
`tag:family` finds them in a search.

In the [slideshow](browsing.md#slideshow), press **T** to tag the photo on screen.

## Best photos first

The sort menu's **Best photos first** puts your best photos on top:

- Once you've rated about 30 photos in Tintype, it learns your taste from your ratings
  and orders the rest by it. The details panel shows its guess for unrated photos
  ("Tintype guesses 3.5 stars, from your ratings"). Ratings read from sidecars don't
  count, since they may come from another program.
- Before that, it uses the **aesthetic score**: turn it on in **Settings** (a one-time
  1.2 GB download, about 0.35 s per photo). It rates how good each photo looks, with an
  open model trained on people's ratings of photos. New photos are scored after each
  scan.

Within a stack of burst frames, the sharpest frame is shown.

## Sidecars

Ratings, tags and people are also saved in an XMP sidecar next to each photo
(`DSCF1234.JPG.xmp`), so they travel with your photos, survive a lost catalog, and can
be read by other programs. Your photos themselves are never changed.

- Sidecars are only created for photos you rate, tag or name someone in.
- Tintype only changes its own fields in a sidecar (rating, keywords and people) and
  keeps everything else as it was.
- If another program changes a sidecar, Tintype reads it on the next scan or launch;
  the sidecar wins.
- Sidecars written by PhotoStat are imported on the first scan, people included.

To stop writing sidecars, turn off **Settings › Save ratings, tags and people in .xmp
sidecars**. Sidecars are still read. Turning it back on writes everything you changed
in the meantime.
