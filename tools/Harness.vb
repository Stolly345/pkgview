' Headless harness around the ORIGINAL pkgmgr.vb/AESEngine.vb (unmodified).
' extractfile() below is Main.vb's extractfile with only the UI lines removed.
Imports System.IO
Imports System.Collections.Generic

Module Main
    Public Extracting As Integer = 1
    Public Cabinet As New List(Of Archivo)
End Module

Module Application
    Sub DoEvents()
    End Sub
End Module

Module Harness
    Dim fd As FileStream

    Sub Run(ByVal args() As String)
        fd = File.Open(args(0), FileMode.Open, FileAccess.Read)
        hdr = ReadPkgHeader(fd)
        fseek(fd, hdr.data_sec_offset)
        readcontent(fd, "ROOT")
        For Each r As Archivo In Cabinet
            Dim folder As String = r.Folder.Replace("\", "/")
            Console.WriteLine(folder & "/" & r.Name & " " & r.Size & " " & Hex(r.Type))
            If (r.Type And &HFF) = 4 Then
                Directory.CreateDirectory(args(1) & "/" & folder & "/" & r.Name)
            Else
                extractfile(args(1) & "/" & folder, r.Name, r.Offset, r.Size, r.Type)
            End If
        Next
        fd.Close()
    End Sub

    Function extractfile(ByVal path As String, ByVal name As String, ByVal offset As UInt64, ByVal size As UInt64, ByVal type As UInt32) As Boolean
        Dim buf() As Byte
        Dim extra As FileStream
        Dim j As UInt64
        If Directory.Exists(path) = False Then Directory.CreateDirectory(path)
        extra = File.Open(path & "/" & name, FileMode.Create, FileAccess.Write)
        AESEngine.free()
        fseek(fd, hdr.data_sec_offset + offset)
        If size >= 512 Then
            For j = 0 To size - 512 Step 512
                buf = decrypt(fd, 32, type And "&HFF000000")
                extra.Write(buf, 0, 512)
            Next
        End If
        Dim siz As UInt64
        If j < size Then
            siz = size - j
            buf = decrypt(fd, ((siz) / 16), type And "&HFF000000")
            If buf.Length < siz Then
                extra.Write(buf, 0, buf.Length)
                siz = siz - buf.Length
                buf = decrypt(fd, 1)
                extra.Write(buf, 0, siz)
            Else
                extra.Write(buf, 0, siz)
            End If
        End If
        extra.Close()
        Return True
    End Function
End Module

Module EntryPoint
    Sub Main(ByVal args() As String)
        Harness.Run(args)
    End Sub
End Module
