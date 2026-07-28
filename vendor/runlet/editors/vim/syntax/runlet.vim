if exists('b:current_syntax')
  finish
endif

syntax case match

syntax match runletComment /#.*$/ contains=@Spell
syntax match runletComment /\/\/.*$/ contains=@Spell
syntax region runletString start=/"/ skip=/\\./ end=/"/ contains=runletEscape
syntax match runletEscape /\\\%(["\\\/bfnrt]\|u[0-9A-Fa-f]\{4}\)/ contained
syntax match runletNumber /\<\d\+\%([eE][+-]\=\d\+\)\>/
syntax match runletNumber /\<\d\+\.\d\+\%([eE][+-]\=\d\+\)\=/

syntax keyword runletKeyword return for in boundary retry catch if else fold skip assert fail
syntax keyword runletOperator and or not
syntax keyword runletBoolean true false
syntax keyword runletConstant null
syntax match runletOperator /==\|!=\|<=\|>=\|=\|<\|>\|+\|-\|\*\|\/\|%/
syntax match runletBinding /^\s*\zs\%([[:alpha:]_]\|[^[:ascii:]]\)\%([[:alnum:]_]\|[^[:ascii:]]\)*\ze\s*=/
syntax match runletFunction /\%([[:alpha:]_]\|[^[:ascii:]]\)\%([[:alnum:]_.]\|[^[:ascii:]]\)*\ze\s*(/
syntax match runletProperty /\.\zs\%([[:alpha:]_]\|[^[:ascii:]]\)\%([[:alnum:]_]\|[^[:ascii:]]\)*/
syntax match runletProperty /\%([[:alpha:]_]\|[^[:ascii:]]\)\%([[:alnum:]_]\|[^[:ascii:]]\)*\ze\s*:/

highlight default link runletComment Comment
highlight default link runletString String
highlight default link runletEscape SpecialChar
highlight default link runletNumber Number
highlight default link runletKeyword Keyword
highlight default link runletOperator Operator
highlight default link runletBoolean Boolean
highlight default link runletConstant Constant
highlight default link runletBinding Identifier
highlight default link runletFunction Function
highlight default link runletProperty Identifier

let b:current_syntax = 'runlet'
