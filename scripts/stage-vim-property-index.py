from pathlib import Path
import subprocess
BASE='0cdb8d4e578066c16af42fa58bde08a2002c6796'
paths=['runtime/vim9/bridge.vim','runtime/vim9/benchmark_scroll.py','.github/workflows/vim9-benchmark.yml']
for path in paths:
    assert Path(path).read_bytes()==subprocess.check_output(['git','show',f'{BASE}:{path}']), path

def replace(text,old,new):
    assert text.count(old)==1, old
    return text.replace(old,new)
p=Path(paths[0]);s=p.read_text()
s=replace(s,'  var properties: list<dict<any>> = []\n','''  var properties: list<dict<any>> = []
  # Index once per frame; scanning the whole property list for every source
  # row makes dense diagnostics cost O(visible_rows * properties). Definitions
  # are frame-local so prop_type_change()/deletion still invalidate immediately.
  var propertyLines: dict<list<any>> = {}
  var propertyDefinitions: dict<dict<any>> = {}
''')
s=replace(s,"""    property.definition = empty(propertyType) ? {}
      : (typeBuffer > 0 ? prop_type_get(propertyType, {bufnr: typeBuffer}) : prop_type_get(propertyType))""","""    var definitionKey = string([typeBuffer, propertyType])
    if !has_key(propertyDefinitions, definitionKey)
      propertyDefinitions[definitionKey] = empty(propertyType) ? {}
        : (typeBuffer > 0 ? prop_type_get(propertyType, {bufnr: typeBuffer}) : prop_type_get(propertyType))
    endif
    property.definition = propertyDefinitions[definitionKey]""")
s=replace(s,'      add(properties, property)\n','''      add(properties, property)
      var key = string(property.lnum)
      if !has_key(propertyLines, key)
        propertyLines[key] = []
      endif
      add(propertyLines[key], property)
''')
s=replace(s,'var lineProperties = filter(copy(properties), (_, item) => item.lnum == lineNumber)',"var lineProperties = get(propertyLines, string(lineNumber), [])")
p.write_text(s)
p=Path(paths[1]);s=p.read_text()
s=replace(s,'def measure(samples, windows, lines):','def measure(samples, windows, lines, properties_per_line=0):')
s=replace(s,'            source.write_text("".join(','            name = \'diagnostic_property_value\' if properties_per_line else \'value\'\n            source.write_text("".join(')
s=replace(s,'f\'let value_{n:05d}', 'f\'let {name}_{n:05d}')
needle='            for _ in range(windows - 1):\n'
s=replace(s,needle,'''            if properties_per_line:
                # All positions are ASCII source-byte boundaries. Bound fixture
                # setup to the measured path plus margin, not the whole file.
                editor.ex("highlight ScrollDiagnostic guifg=#d08a34 guisp=#d08a34 gui=undercurl")
                editor.ex("call prop_type_add('ScrollDiagnostic', {'highlight':'ScrollDiagnostic'})")
                properties = root / "properties.vim"
                properties.write_text(
                    f"for row in range(1,{min(lines, samples + 256)})\\n"
                    f" for column in range(1,{properties_per_line})\\n"
                    "  call prop_add(row,column,{'type':'ScrollDiagnostic','length':1})\\n"
                    " endfor\\nendfor\\n", encoding="utf-8")
                editor.ex("source " + str(properties))
''' + needle)
s=replace(s,'                "buffer_lines": lines,','                "buffer_lines": lines,\n                "properties_per_line": properties_per_line,')
s=replace(s,'    args = parser.parse_args()','''    parser.add_argument("--properties-per-line", type=int, choices=range(33), default=0,
                        metavar="0..32", help="Dense diagnostic-span workload; 0 retains the original benchmark")
    args = parser.parse_args()''')
s=replace(s,'measure(args.samples, windows, args.lines)','measure(args.samples, windows, args.lines, args.properties_per_line)')
p.write_text(s)
p=Path(paths[2]);s=p.read_text()
needle='      - name: Exercise one and two split workloads against real Vim\n'
s=replace(s,needle,'''      - name: Verify live property definitions and split paint isolation
        run: python3 runtime/vim9/test_render_property_index.py -v
''' + needle)
needle='          {\n            echo \'## Semantic benchmark smoke results\''
s=replace(s,needle,'''          python3 runtime/vim9/benchmark_scroll.py --lines 1000 --samples 5 --windows 1 2 --properties-per-line 24 | tee "$RUNNER_TEMP/vim9-dense-scroll-benchmark.json"
''' + needle)
p.write_text(s)
