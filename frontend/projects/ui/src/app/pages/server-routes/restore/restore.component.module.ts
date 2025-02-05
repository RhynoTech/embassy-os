import { NgModule } from '@angular/core'
import { RouterModule, Routes } from '@angular/router'
import { CommonModule } from '@angular/common'
import { IonicModule } from '@ionic/angular'
import { RestorePage } from './restore.component'
import { SharingModule } from 'src/app/modules/sharing.module'
import { BackupDrivesComponentModule } from 'src/app/components/backup-drives/backup-drives.component.module'
import { AppRecoverSelectPageModule } from 'src/app/modals/app-recover-select/app-recover-select.module'

const routes: Routes = [
  {
    path: '',
    component: RestorePage,
  },
]

@NgModule({
  imports: [
    CommonModule,
    IonicModule,
    RouterModule.forChild(routes),
    SharingModule,
    BackupDrivesComponentModule,
    AppRecoverSelectPageModule,
  ],
  declarations: [
    RestorePage,
  ],
})
export class RestorePageModule { }