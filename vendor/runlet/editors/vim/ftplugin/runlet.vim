if exists('b:did_ftplugin')
  finish
endif
let b:did_ftplugin = 1

setlocal commentstring=//\ %s
setlocal comments=b:#,b://
setlocal suffixesadd=.rnlt

let b:undo_ftplugin = 'setlocal commentstring< comments< suffixesadd<'
