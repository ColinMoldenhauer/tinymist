/// compile: true

#set page(width: 200pt, height: 120pt, margin: 10pt)

#let slide-header(section, title) = stack(
  spacing: 0.3cm,
  text(size: 14pt)[#section],
  [= #title],
)

#let image-slide(section, title, body) = {
  grid(
    rows: (auto, 1fr),
    row-gutter: 0.4cm,
    slide-header(section, title),
    rect(width: 100%, height: 100%, fill: luma(230))[#body],
  )
}

#let soil = "Soil properties"
#image-slide(soil, [Distributions], "body as string")
#pagebreak()
#image-slide([#soil], [Distributions], [body as content])
