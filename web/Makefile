# Builds site/ from src/ using the-proportional-web's toolchain (pandoc,
# esbuild, html-minifier — all provided by `nix develop`). Built artifacts in
# site/ are committed; deploy.sh serves them as-is, so run `make` and commit
# before pushing.
VERSION=$(shell jq -r .version package.json)
DATE=$(shell date +%F)

all: site/index.html site/bibliography/index.html site/reference/index.html site/index.css site/src/demo/demo.css

# The Reference: the Book's template and style sheets, with a table of
# contents (unnumbered: its headings are looked up, not read in order).
site/reference/index.html: src/reference/index.md Makefile src/demo/template.html site/index.css site/src/demo/demo.css site/index.js
	@mkdir -p site/reference
	pandoc \
		--toc --toc-depth=2 \
		-s \
		--css "/index.css?v=$$(sha256sum site/index.css | cut -c1-12)" \
		--css "/src/demo/demo.css?v=$$(sha256sum site/src/demo/demo.css | cut -c1-12)" \
		-M document-css=false \
		-V 'header-includes=<script src="/index.js?v='"$$(sha256sum site/index.js | cut -c1-12)"'"></script>' \
		--template=src/demo/template.html \
		-i $< \
		| html-minifier --collapse-whitespace > $@

clean:
	rm -f site/index.html site/bibliography/index.html site/reference/index.html site/index.css site/src/demo/demo.css

# Asset URLs carry a content hash so a changed stylesheet can never be served
# from a stale browser cache. Cloudflare rewrites the origin's `Cache-Control:
# no-cache` to max-age=14400 on .css/.js (but not on HTML), so without this the
# browser pairs fresh markup with up-to-4-hour-old CSS. The hashes are computed
# in the recipe, after the prerequisites below have been rebuilt.
site/index.html: src/index.md Makefile src/demo/template.html site/index.css site/src/demo/demo.css site/index.js
	pandoc \
		--toc --toc-depth=2 \
		-s \
		--number-sections --number-offset=0 \
		--css "/index.css?v=$$(sha256sum site/index.css | cut -c1-12)" \
		--css "/src/demo/demo.css?v=$$(sha256sum site/src/demo/demo.css | cut -c1-12)" \
		-M document-css=false \
		-V 'header-includes=<script src="/index.js?v='"$$(sha256sum site/index.js | cut -c1-12)"'"></script>' \
		--template=src/demo/template.html \
		-Vversion=v$(VERSION) -Vdate=$(DATE) \
		-i $< \
		| html-minifier --collapse-whitespace > $@

site/bibliography/index.html: src/bibliography/index.md Makefile src/demo/template.html site/index.css
	@mkdir -p site/bibliography
	pandoc \
		-s \
		--css "/index.css?v=$$(sha256sum site/index.css | cut -c1-12)" \
		-M document-css=false \
		--template=src/demo/template.html \
		-i $< \
		| html-minifier --collapse-whitespace > $@

site/index.css: src/index.css $(wildcard src/*.css)
	echo "/* By Oskar Wickström\nLicensed under the MIT License (https://github.com/owickstrom/the-proportional-web/blob/main/LICENSE.md)\n*/" > $@
	esbuild --bundle --external:/Fern-Roman-VF.woff2 --external:/Fern-Italic-VF.woff2 $< >> $@

site/src/demo/demo.css: src/demo/demo.css
	@mkdir -p site/src/demo
	cp $< $@

dev: all
	@concurrently -n serve,watch \
		"browser-sync start --server site --port 8000 --no-open --files 'site/**/*'" \
		"watchexec -w src -e css,html,js,md make"

.PHONY: all clean dev
